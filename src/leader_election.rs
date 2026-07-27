use std::error::Error as _;
use std::future::Future;
use std::pin::pin;
use std::time::{Duration, Instant};

use futures::future::{self, Either};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ObjectMeta, PostParams};
use rand::{Rng, rng};
use tracing::{debug, error, info, warn};

/// Lease-based leader election, allowing multiple replicas of a controller
/// to run while ensuring that only one of them is reconciling at a time.
///
/// This uses a `coordination.k8s.io/v1` [`Lease`] object, following the same
/// protocol (and using the same default timings) as the Kubernetes client-go
/// `leaderelection` package: the leader repeatedly renews the lease, and
/// other candidates take the lease over if the leader fails to renew it for
/// `lease_duration`. Expiry is determined by observing the lease go
/// unchanged for `lease_duration`, rather than by comparing timestamps in
/// the lease against the local clock, so it is robust to clock skew between
/// candidates. Note that this protocol is cooperative: it guarantees mutual
/// exclusion only among candidates that respect the lease.
///
/// The controller's service account needs `get`, `create`, and `update`
/// permissions on `leases` in the `coordination.k8s.io` API group for this
/// to work.
///
/// The `identity` must be non-empty and unique among the candidates for a
/// given lease; the pod name (available in the `HOSTNAME` environment
/// variable, or via the downward API) is a good choice. Beware that
/// candidates for the same lease which share an identity will each mistake
/// the other's renewals for their own and all act as leader simultaneously,
/// so never create multiple `LeaderElection`s in the same process with the
/// same lease name and identity. To have one lease guard several
/// controllers in a process, share a single `LeaderElection` via
/// [`with_lease`](LeaderElection::with_lease) instead.
#[derive(Clone)]
pub struct LeaderElection {
    api: Api<Lease>,
    lease_name: String,
    identity: String,
    lease_duration: Duration,
    renew_deadline: Duration,
    retry_period: Duration,
}

impl LeaderElection {
    /// Creates a leader election configuration for the [`Lease`] named
    /// `lease_name` in the given `namespace`, identifying this instance of
    /// the controller as `identity`. The timings default to the client-go
    /// defaults: a lease duration of 15 seconds, a renew deadline of 10
    /// seconds, and a retry period of 2 seconds.
    pub fn new(client: Client, namespace: &str, lease_name: &str, identity: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            lease_name: lease_name.to_owned(),
            identity: identity.to_owned(),
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
        }
    }

    /// Sets how long a non-leader must wait after the last observed change
    /// to the lease before forcibly taking it over. Larger values slow down
    /// failover; smaller values increase the risk that a leader which is
    /// still running (but partitioned from the API server) has not yet
    /// stopped reconciling when the new leader starts. Must be greater than
    /// the renew deadline.
    pub fn with_lease_duration(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration;
        self
    }

    /// Sets how long the leader will keep trying to renew the lease before
    /// giving up leadership. Must be less than the lease duration (so that
    /// a leader which cannot reach the API server gives up before another
    /// candidate can take the lease over) and greater than the retry
    /// period.
    pub fn with_renew_deadline(mut self, renew_deadline: Duration) -> Self {
        self.renew_deadline = renew_deadline;
        self
    }

    /// Sets how often candidates poll the lease while waiting to acquire
    /// it, and how often the leader renews it.
    pub fn with_retry_period(mut self, retry_period: Duration) -> Self {
        self.retry_period = retry_period;
        self
    }

    /// Wait until this instance holds the lease, then run `fut` while
    /// renewing the lease in the background.
    ///
    /// If leadership is lost (because the lease could not be renewed in
    /// time, or was taken over by another candidate), `fut` is dropped,
    /// cancelling its work, and this method returns `None`. The caller
    /// should then promptly either exit the process (letting Kubernetes
    /// restart it) or rejoin the election by calling this method again.
    /// Note that dropping `fut` cancels it cooperatively: work it has
    /// spawned as separate tasks, or blocking code, is not cancelled. If
    /// `fut` does such things, prefer exiting the process so that no work
    /// outlives the lease.
    ///
    /// If `fut` completes on its own, the lease is voluntarily released
    /// (handing leadership over immediately rather than making the other
    /// candidates wait for it to expire) and its output is returned.
    ///
    /// To run a [`Controller`](crate::Controller) under a lease, pass its
    /// [`run`](crate::Controller::run) future; to have one lease guard
    /// several controllers (rather than electing a separate leader per
    /// controller), pass a future that runs all of them, for instance:
    ///
    /// ```ignore
    /// leader_election
    ///     .with_lease(futures::future::join(controller_a.run(), controller_b.run()))
    ///     .await;
    /// ```
    ///
    /// During graceful shutdown (for instance, on receiving a termination
    /// signal), drop the future returned by this method to stop its work,
    /// then call [`release`](LeaderElection::release) on a clone of this
    /// `LeaderElection` to hand leadership over immediately rather than
    /// making the other replicas wait for the lease to expire.
    ///
    /// Panics if the configured timings are inconsistent or the identity is
    /// empty.
    pub async fn with_lease<F: Future>(&self, fut: F) -> Option<F::Output> {
        let acquired_at = self.acquire().await;
        let fut = pin!(fut);
        let lost = pin!(self.hold(acquired_at));
        match future::select(fut, lost).await {
            Either::Left((output, _)) => {
                self.release().await;
                Some(output)
            }
            Either::Right(((), _)) => None,
        }
    }

    fn validate(&self) {
        assert!(!self.identity.is_empty(), "identity must not be empty");
        assert!(
            self.renew_deadline < self.lease_duration,
            "renew_deadline must be less than lease_duration"
        );
        assert!(
            self.retry_period < self.renew_deadline,
            "retry_period must be less than renew_deadline"
        );
        assert!(
            i32::try_from(self.lease_duration.as_secs()).is_ok(),
            "lease_duration must be at most i32::MAX seconds"
        );
    }

    fn lease_duration_seconds(&self) -> i32 {
        i32::try_from(self.lease_duration.as_secs())
            .expect("lease_duration must be at most i32::MAX seconds")
    }

    /// Wait until we hold the lease, returning the instant captured just
    /// before the request that acquired it was sent (the pessimistic time
    /// from which renewal deadlines must be measured; see [`hold`]). Panics
    /// if the configured timings are inconsistent or the identity is empty.
    ///
    /// [`hold`]: LeaderElection::hold
    async fn acquire(&self) -> Instant {
        self.validate();
        info!(
            lease_name = %self.lease_name,
            identity = %self.identity,
            "attempting to acquire leadership lease",
        );
        let mut observed = None;
        loop {
            let started = Instant::now();
            match self.try_acquire(&mut observed).await {
                Ok(true) => {
                    info!(
                        lease_name = %self.lease_name,
                        identity = %self.identity,
                        "acquired leadership lease",
                    );
                    return started;
                }
                Ok(false) => {
                    debug!(
                        lease_name = %self.lease_name,
                        "leadership lease is held by another candidate",
                    );
                }
                // a 401 or 403 will never resolve on its own; it almost
                // always means the service account is missing RBAC
                // permissions on leases, so log it more loudly
                Err(kube::Error::Api(e)) if e.code == 401 || e.code == 403 => {
                    error!(
                        error = %e,
                        lease_name = %self.lease_name,
                        "not permitted to access the leadership lease; the \
                         service account needs get, create, and update \
                         permissions on leases in coordination.k8s.io",
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        source = e.source(),
                        lease_name = %self.lease_name,
                        "error while trying to acquire leadership lease",
                    );
                }
            }
            // jitter the retry period to avoid thundering herds. the rng is
            // bound separately because holding a `ThreadRng` (which is not
            // `Send`) across the await point would make this future `!Send`
            let jitter = rng().random_range(1.0..1.5);
            tokio::time::sleep(self.retry_period.mul_f64(jitter)).await;
        }
    }

    /// Try once to acquire the lease, returning whether we now hold it.
    /// `observed` tracks the last seen state of the lease and when we saw
    /// it, so that expiry is measured against our own clock.
    async fn try_acquire(
        &self,
        observed: &mut Option<(LeaseSpec, Instant)>,
    ) -> Result<bool, kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.lease_name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.lease_name.clone()),
                    ..Default::default()
                },
                spec: Some(next_spec(
                    &self.identity,
                    self.lease_duration_seconds(),
                    None,
                    Timestamp::now(),
                )),
            };
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                // another candidate created the lease first
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                Err(e) => Err(e),
            };
        };

        let spec = lease.spec.take().unwrap_or_default();
        // a missing or empty holder means the lease was voluntarily
        // released (client-go writes an empty string on release) and can be
        // taken immediately
        let holder = spec.holder_identity.as_deref().filter(|h| !h.is_empty());
        let held_by_us = holder == Some(self.identity.as_str());
        if !held_by_us && holder.is_some() {
            if observed.as_ref().is_none_or(|(last, _)| *last != spec) {
                *observed = Some((spec, Instant::now()));
                return Ok(false);
            }
            let (_, observed_at) = observed.as_ref().unwrap();
            if observed_at.elapsed() < self.lease_duration {
                return Ok(false);
            }
            // the holder has failed to renew the lease for a full
            // lease_duration, so we can take it over
        }
        lease.spec = Some(next_spec(
            &self.identity,
            self.lease_duration_seconds(),
            Some(&spec),
            Timestamp::now(),
        ));
        // replace (rather than patch) so that the write fails with a
        // conflict if another candidate updated the lease since we read it
        match self
            .api
            .replace(&self.lease_name, &PostParams::default(), &lease)
            .await
        {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Renew the lease until we lose it, then return. Only call this while
    /// holding the lease. `last_renew` must be an instant captured *before*
    /// the request that last renewed (or acquired) the lease was sent.
    ///
    /// Renewals are measured from before their request is sent: other
    /// candidates start their takeover clocks when they observe the written
    /// lease, which can happen well before we receive the response, so
    /// measuring from the response could extend our renew deadline past
    /// their takeover time.
    async fn hold(&self, mut last_renew: Instant) {
        let mut interval = tokio::time::interval(self.retry_period);
        // the first tick completes immediately, and the lease was just
        // renewed by acquiring it
        interval.tick().await;
        loop {
            // an interval (rather than a sleep) keeps renewals at a
            // consistent cadence even when an attempt is slow, rather than
            // eating into the renew deadline budget between attempts
            interval.tick().await;
            // bound each attempt by the time remaining until the renew
            // deadline, so that a hung request (the kube client's default
            // read timeout is much longer than the deadline) can't keep us
            // acting as leader after another candidate may have taken over
            let remaining = self.renew_deadline.saturating_sub(last_renew.elapsed());
            let started = Instant::now();
            match tokio::time::timeout(remaining, self.renew()).await {
                Ok(Ok(true)) => last_renew = started,
                Ok(Ok(false)) => {
                    warn!(
                        lease_name = %self.lease_name,
                        identity = %self.identity,
                        "leadership lease was taken by another candidate",
                    );
                    return;
                }
                Ok(Err(e)) => {
                    warn!(
                        error = %e,
                        source = e.source(),
                        lease_name = %self.lease_name,
                        "failed to renew leadership lease",
                    );
                    if last_renew.elapsed() >= self.renew_deadline {
                        warn!(
                            lease_name = %self.lease_name,
                            identity = %self.identity,
                            "failed to renew leadership lease within the renew deadline; giving up leadership",
                        );
                        return;
                    }
                }
                Err(_) => {
                    warn!(
                        lease_name = %self.lease_name,
                        identity = %self.identity,
                        "leadership lease renewal did not complete within the renew deadline; giving up leadership",
                    );
                    return;
                }
            }
        }
    }

    /// Try once to renew the lease, returning whether we still hold it.
    async fn renew(&self) -> Result<bool, kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.lease_name).await? else {
            return Ok(false);
        };
        let spec = lease.spec.take().unwrap_or_default();
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return Ok(false);
        }
        lease.spec = Some(next_spec(
            &self.identity,
            self.lease_duration_seconds(),
            Some(&spec),
            Timestamp::now(),
        ));
        self.api
            .replace(&self.lease_name, &PostParams::default(), &lease)
            .await?;
        Ok(true)
    }

    /// Voluntarily release the lease if we hold it, allowing another
    /// candidate to take it over immediately rather than waiting for it to
    /// expire. Call this during graceful shutdown, after the controller has
    /// stopped reconciling (for instance, after the future returned by
    /// [`with_lease`](LeaderElection::with_lease) has been dropped in
    /// response to a termination signal). This is best-effort: errors
    /// are logged and ignored, since the lease will expire on its own
    /// regardless.
    pub async fn release(&self) {
        match self.try_release().await {
            Ok(true) => {
                info!(
                    lease_name = %self.lease_name,
                    identity = %self.identity,
                    "released leadership lease",
                );
            }
            Ok(false) => {
                debug!(
                    lease_name = %self.lease_name,
                    "leadership lease is not held by us; nothing to release",
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    source = e.source(),
                    lease_name = %self.lease_name,
                    "failed to release leadership lease",
                );
            }
        }
    }

    /// Try once to release the lease, returning whether we released it.
    async fn try_release(&self) -> Result<bool, kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.lease_name).await? else {
            return Ok(false);
        };
        let spec = lease.spec.take().unwrap_or_default();
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return Ok(false);
        }
        lease.spec = Some(released_spec(&spec));
        match self
            .api
            .replace(&self.lease_name, &PostParams::default(), &lease)
            .await
        {
            Ok(_) => Ok(true),
            // another candidate already took the lease over
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Compute the lease spec that makes `identity` the holder, given the
/// previous spec (if the lease already existed).
fn next_spec(
    identity: &str,
    lease_duration_seconds: i32,
    prev: Option<&LeaseSpec>,
    now: Timestamp,
) -> LeaseSpec {
    let now = MicroTime(now);
    let held_by_us = prev.is_some_and(|s| s.holder_identity.as_deref() == Some(identity));
    LeaseSpec {
        holder_identity: Some(identity.to_owned()),
        lease_duration_seconds: Some(lease_duration_seconds),
        acquire_time: if held_by_us {
            prev.and_then(|s| s.acquire_time.clone())
        } else {
            Some(now.clone())
        },
        renew_time: Some(now),
        lease_transitions: match prev {
            None => Some(0),
            Some(s) if held_by_us => s.lease_transitions,
            Some(s) => Some(s.lease_transitions.unwrap_or(0).saturating_add(1)),
        },
        ..Default::default()
    }
}

/// Compute the lease spec that marks the lease as no longer held,
/// preserving the transition count for the next holder to increment.
fn released_spec(prev: &LeaseSpec) -> LeaseSpec {
    LeaseSpec {
        holder_identity: None,
        lease_transitions: prev.lease_transitions,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-07-22T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn fresh_acquire() {
        let spec = next_spec("us", 15, None, now());
        assert_eq!(spec.holder_identity.as_deref(), Some("us"));
        assert_eq!(spec.lease_duration_seconds, Some(15));
        assert_eq!(spec.acquire_time, Some(MicroTime(now())));
        assert_eq!(spec.renew_time, Some(MicroTime(now())));
        assert_eq!(spec.lease_transitions, Some(0));
    }

    #[test]
    fn renewal_preserves_acquire_time_and_transitions() {
        let acquired: Timestamp = "2026-07-21T00:00:00Z".parse().unwrap();
        let prev = LeaseSpec {
            holder_identity: Some("us".to_owned()),
            lease_duration_seconds: Some(15),
            acquire_time: Some(MicroTime(acquired)),
            renew_time: Some(MicroTime(acquired)),
            lease_transitions: Some(3),
            ..Default::default()
        };
        let spec = next_spec("us", 15, Some(&prev), now());
        assert_eq!(spec.holder_identity.as_deref(), Some("us"));
        assert_eq!(spec.acquire_time, Some(MicroTime(acquired)));
        assert_eq!(spec.renew_time, Some(MicroTime(now())));
        assert_eq!(spec.lease_transitions, Some(3));
    }

    #[test]
    fn takeover_increments_transitions() {
        let prev = LeaseSpec {
            holder_identity: Some("them".to_owned()),
            lease_duration_seconds: Some(15),
            acquire_time: Some(MicroTime(now())),
            renew_time: Some(MicroTime(now())),
            lease_transitions: Some(3),
            ..Default::default()
        };
        let spec = next_spec("us", 15, Some(&prev), now());
        assert_eq!(spec.holder_identity.as_deref(), Some("us"));
        assert_eq!(spec.acquire_time, Some(MicroTime(now())));
        assert_eq!(spec.lease_transitions, Some(4));
    }

    #[test]
    fn release_clears_holder_and_preserves_transitions() {
        let prev = LeaseSpec {
            holder_identity: Some("us".to_owned()),
            lease_duration_seconds: Some(15),
            acquire_time: Some(MicroTime(now())),
            renew_time: Some(MicroTime(now())),
            lease_transitions: Some(3),
            ..Default::default()
        };
        let spec = released_spec(&prev);
        assert_eq!(spec.holder_identity, None);
        assert_eq!(spec.lease_transitions, Some(3));
        assert_eq!(spec.renew_time, None);
    }
}
