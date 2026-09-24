//! Maintaining the standard `status.conditions` of a resource.
//!
//! These functions operate on the [`Condition`] type that Kubernetes defines for this purpose, following the same rules
//! as `k8s.io/apimachinery/pkg/api/meta.SetStatusCondition`:
//!
//! * A resource has at most one condition of each `type`.
//! * `lastTransitionTime` records when the condition's `status` last
//!   changed. It does not change when only the `reason`, `message`, or
//!   `observedGeneration` do, so it answers "how long has this been
//!   `False`?" rather than "when was this last written?".
//! * `observedGeneration` records the `metadata.generation` of the resource
//!   that the condition was determined from. A condition whose
//!   `observedGeneration` is older than the resource's current generation
//!   describes a spec that has since been changed, so should not be trusted
//!   as describing the current one (see [`find_observed`]).
//!
//! A reconciler typically computes each condition from what it observed,
//! applies them with [`set`], and writes the status back only if any of
//! them changed:
//!
//! ```no_run
//! # use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
//! # use k8s_controller::conditions::{self, ConditionStatus, DesiredCondition};
//! # struct Status { conditions: Vec<Condition> }
//! # fn f(status: &mut Status, generation: Option<i64>, ready: bool) {
//! let change = conditions::set(
//!     &mut status.conditions,
//!     if ready {
//!         DesiredCondition::new("Ready", ConditionStatus::True, "DeploymentAvailable", "")
//!     } else {
//!         DesiredCondition::new(
//!             "Ready",
//!             ConditionStatus::False,
//!             "DeploymentUnavailable",
//!             "waiting for the deployment's pods to become ready",
//!         )
//!     }
//!     .observed_generation(generation),
//! );
//! if change.is_changed() {
//!     // write the status
//! }
//! # }
//! ```

use std::fmt::Display;
use std::str::FromStr;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::jiff::Timestamp;

/// The `status` of a [`Condition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConditionStatus {
    True,
    False,
    /// The controller cannot currently determine whether the condition
    /// holds, for instance because it has not yet observed the resource it
    /// depends on.
    Unknown,
}

impl ConditionStatus {
    /// The value of the `status` field for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            ConditionStatus::True => "True",
            ConditionStatus::False => "False",
            ConditionStatus::Unknown => "Unknown",
        }
    }
}

impl From<bool> for ConditionStatus {
    fn from(value: bool) -> Self {
        if value {
            ConditionStatus::True
        } else {
            ConditionStatus::False
        }
    }
}

impl Display for ConditionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when parsing a string that is not `True`, `False`, or
/// `Unknown` as a [`ConditionStatus`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid condition status {0:?}")]
pub struct InvalidConditionStatus(String);

impl FromStr for ConditionStatus {
    type Err = InvalidConditionStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "True" => Ok(ConditionStatus::True),
            "False" => Ok(ConditionStatus::False),
            "Unknown" => Ok(ConditionStatus::Unknown),
            _ => Err(InvalidConditionStatus(s.to_owned())),
        }
    }
}

/// A condition as a reconciler wants it to be, to be applied with [`set`].
///
/// This omits `lastTransitionTime`, which [`set`] maintains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredCondition {
    /// The condition's type, in `CamelCase` or `foo.example.com/CamelCase`.
    pub type_: String,
    pub status: ConditionStatus,
    /// A machine-readable `CamelCase` identifier for why the condition has
    /// this status. Must not be empty: the API server rejects conditions
    /// with an empty reason in resources that use the standard condition
    /// schema.
    pub reason: String,
    /// A human-readable explanation, which may be empty.
    pub message: String,
    /// The `metadata.generation` of the resource that this condition was
    /// determined from. See [`observed_generation`](Self::observed_generation).
    pub observed_generation: Option<i64>,
}

impl DesiredCondition {
    /// Creates a desired condition with no observed generation.
    pub fn new(
        type_: impl Into<String>,
        status: ConditionStatus,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            type_: type_.into(),
            status,
            reason: reason.into(),
            message: message.into(),
            observed_generation: None,
        }
    }

    /// Sets the generation this condition was determined from. This should
    /// be the `metadata.generation` of the resource as it was passed to the
    /// reconciler, not as it is when the status is written, since the spec
    /// may have changed in between.
    pub fn observed_generation(mut self, generation: Option<i64>) -> Self {
        self.observed_generation = generation;
        self
    }
}

/// What [`set`] changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[must_use = "a status only needs to be written if its conditions changed"]
pub enum ConditionChange {
    /// The condition was already exactly as desired.
    Unchanged,
    /// The condition's `reason`, `message`, or `observedGeneration` changed,
    /// but its `status` did not, so its `lastTransitionTime` was kept.
    Updated,
    /// The condition was added, or its `status` changed, and its
    /// `lastTransitionTime` was set to the current time.
    Transitioned,
}

impl ConditionChange {
    /// Whether anything changed, and so whether the status needs writing.
    pub fn is_changed(self) -> bool {
        self != ConditionChange::Unchanged
    }
}

/// Returns the condition of the given type, if present.
pub fn find<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

/// Returns the condition of the given type, if present and determined from
/// the given `generation` of its resource (which should be that resource's
/// current `metadata.generation`).
///
/// Use this rather than [`find`] when reading the conditions of a resource
/// that another controller maintains: until that controller has reconciled
/// the latest change to the resource's spec, its conditions describe the
/// previous spec.
pub fn find_observed<'a>(
    conditions: &'a [Condition],
    type_: &str,
    generation: Option<i64>,
) -> Option<&'a Condition> {
    find(conditions, type_).filter(|c| c.observed_generation == generation)
}

/// Sets the condition of `desired.type_` to `desired`, adding it if it is
/// not already present, and returns what changed.
///
/// Its `lastTransitionTime` is set to the current time if the condition is
/// added or its status changes, and otherwise kept.
pub fn set(conditions: &mut Vec<Condition>, desired: DesiredCondition) -> ConditionChange {
    set_at(conditions, desired, Timestamp::now())
}

fn set_at(
    conditions: &mut Vec<Condition>,
    desired: DesiredCondition,
    now: Timestamp,
) -> ConditionChange {
    // Kubernetes serializes times at second precision, so a finer time would
    // differ from the same condition read back from the API server.
    let now = Time(Timestamp::from_second(now.as_second()).expect("in range"));
    let DesiredCondition {
        type_,
        status,
        reason,
        message,
        observed_generation,
    } = desired;
    let status = status.as_str();

    let Some(existing) = conditions.iter_mut().find(|c| c.type_ == type_) else {
        conditions.push(Condition {
            type_,
            status: status.to_owned(),
            reason,
            message,
            observed_generation,
            last_transition_time: now,
        });
        return ConditionChange::Transitioned;
    };

    if existing.status != status {
        existing.status = status.to_owned();
        existing.reason = reason;
        existing.message = message;
        existing.observed_generation = observed_generation;
        existing.last_transition_time = now;
        return ConditionChange::Transitioned;
    }

    if existing.reason == reason
        && existing.message == message
        && existing.observed_generation == observed_generation
    {
        return ConditionChange::Unchanged;
    }
    existing.reason = reason;
    existing.message = message;
    existing.observed_generation = observed_generation;
    ConditionChange::Updated
}

/// Removes the condition of the given type, returning whether it was
/// present.
pub fn remove(conditions: &mut Vec<Condition>, type_: &str) -> bool {
    let len = conditions.len();
    conditions.retain(|c| c.type_ != type_);
    conditions.len() != len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn ready(status: ConditionStatus, reason: &str) -> DesiredCondition {
        DesiredCondition::new("Ready", status, reason, "").observed_generation(Some(1))
    }

    #[test]
    fn adds_missing_condition() {
        let mut conditions = vec![];
        let change = set_at(
            &mut conditions,
            ready(ConditionStatus::False, "Starting"),
            at("2026-09-23T00:00:00.75Z"),
        );
        assert_eq!(change, ConditionChange::Transitioned);
        assert_eq!(
            conditions,
            [Condition {
                type_: "Ready".to_owned(),
                status: "False".to_owned(),
                reason: "Starting".to_owned(),
                message: String::new(),
                observed_generation: Some(1),
                last_transition_time: Time(at("2026-09-23T00:00:00Z")),
            }]
        );
    }

    #[test]
    fn keeps_transition_time_unless_status_changes() {
        let mut conditions = vec![];
        let _ = set_at(
            &mut conditions,
            ready(ConditionStatus::False, "Starting"),
            at("2026-09-23T00:00:00Z"),
        );

        let change = set_at(
            &mut conditions,
            ready(ConditionStatus::False, "Starting"),
            at("2026-09-23T00:01:00Z"),
        );
        assert_eq!(change, ConditionChange::Unchanged);

        let change = set_at(
            &mut conditions,
            ready(ConditionStatus::False, "WaitingForPods").observed_generation(Some(2)),
            at("2026-09-23T00:02:00Z"),
        );
        assert_eq!(change, ConditionChange::Updated);
        assert_eq!(conditions[0].reason, "WaitingForPods");
        assert_eq!(conditions[0].observed_generation, Some(2));
        assert_eq!(
            conditions[0].last_transition_time,
            Time(at("2026-09-23T00:00:00Z"))
        );

        let change = set_at(
            &mut conditions,
            ready(ConditionStatus::True, "Available"),
            at("2026-09-23T00:03:00Z"),
        );
        assert_eq!(change, ConditionChange::Transitioned);
        assert_eq!(conditions[0].status, "True");
        assert_eq!(conditions[0].observed_generation, Some(1));
        assert_eq!(
            conditions[0].last_transition_time,
            Time(at("2026-09-23T00:03:00Z"))
        );
    }

    #[test]
    fn leaves_other_conditions_alone() {
        let mut conditions = vec![];
        let t0 = at("2026-09-23T00:00:00Z");
        let _ = set_at(
            &mut conditions,
            ready(ConditionStatus::True, "Available"),
            t0,
        );
        let _ = set_at(
            &mut conditions,
            DesiredCondition::new("Degraded", ConditionStatus::False, "Healthy", ""),
            t0,
        );
        let _ = set_at(
            &mut conditions,
            DesiredCondition::new("Degraded", ConditionStatus::True, "ReplicaLost", ""),
            at("2026-09-23T00:05:00Z"),
        );
        assert_eq!(conditions.len(), 2);
        assert_eq!(conditions[0].type_, "Ready");
        assert_eq!(conditions[0].last_transition_time, Time(t0));
        assert_eq!(conditions[1].status, "True");

        assert!(remove(&mut conditions, "Degraded"));
        assert!(!remove(&mut conditions, "Degraded"));
        assert_eq!(conditions.len(), 1);
    }

    #[test]
    fn find_observed_ignores_stale_conditions() {
        let mut conditions = vec![];
        let _ = set(&mut conditions, ready(ConditionStatus::True, "Available"));
        assert!(find_observed(&conditions, "Ready", Some(1)).is_some());
        assert!(find_observed(&conditions, "Ready", Some(2)).is_none());
        assert!(find_observed(&conditions, "Missing", Some(1)).is_none());
    }

    #[test]
    fn parses_status() {
        for status in [
            ConditionStatus::True,
            ConditionStatus::False,
            ConditionStatus::Unknown,
        ] {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        assert!("true".parse::<ConditionStatus>().is_err());
    }
}
