# Changelog

## Unreleased

## [0.14.1](https://github.com/MaterializeInc/k8s-controller/compare/v0.14.0...v0.14.1) - 2026-10-19

### Other

- Bump serde_json from 1.0.152 to 1.0.154 ([#75](https://github.com/MaterializeInc/k8s-controller/pull/75))
- Bump tokio from 1.53.2 to 1.53.3 ([#74](https://github.com/MaterializeInc/k8s-controller/pull/74))
- Log reconciliation conflicts at info instead of error ([#73](https://github.com/MaterializeInc/k8s-controller/pull/73))

### Changed

* A reconciliation that fails because the object changed since it was read
  is logged at `info` instead of `error`. This covers a `409 Conflict` from
  the context's own writes and a failed `test` operation in the finalizer
  patch. `Error::is_conflict` exposes the classification.

## [0.14.0](https://github.com/MaterializeInc/k8s-controller/compare/v0.13.0...v0.14.0) - 2026-10-14

### Added

- [**breaking**] update kube to 4.2 and k8s-openapi to 0.28 ([#55](https://github.com/MaterializeInc/k8s-controller/pull/55))

### Other

- Bump serde from 1.0.228 to 1.0.229 ([#65](https://github.com/MaterializeInc/k8s-controller/pull/65))
- Bump futures from 0.3.32 to 0.3.34 ([#64](https://github.com/MaterializeInc/k8s-controller/pull/64))
- Bump thiserror from 2.0.18 to 2.0.21 ([#63](https://github.com/MaterializeInc/k8s-controller/pull/63))
- Bump async-trait from 0.1.89 to 0.1.92 ([#67](https://github.com/MaterializeInc/k8s-controller/pull/67))
- Bump rand from 0.9.4 to 0.10.3 ([#69](https://github.com/MaterializeInc/k8s-controller/pull/69))
- Bump serde_json from 1.0.150 to 1.0.152 ([#61](https://github.com/MaterializeInc/k8s-controller/pull/61))
- Bump tokio from 1.52.3 to 1.53.2 ([#62](https://github.com/MaterializeInc/k8s-controller/pull/62))
- Bump http from 1.4.2 to 1.5.0 ([#60](https://github.com/MaterializeInc/k8s-controller/pull/60))
- Fall back to an event.<hex> name when the regarding name can't form one
- Name events explicitly instead of with an invalid generateName

### Fixed

* `EventRecorder` failed to create any event: the API server rejected the
  `<name>.` `generateName` it used with a 422, since a `generateName` must
  itself be a valid DNS subdomain apart from a trailing `-`. Events are now
  named `<name>.<hex nanoseconds>`, as client-go names them, or
  `event.<hex nanoseconds>` when that would not be a valid DNS subdomain
  (for instance, for RBAC objects named like `system:foo`).

## [0.13.0] - 2026-09-29

### Added

* Reconciliation metrics. `Controller::with_observer` reports every
  reconciliation pass to a `ReconcileObserver`, with its `Phase` (`init`,
  `apply`, `cleanup`, or `delete`), `Outcome` (`completed`, `waiting`,
  `skipped`, `failed`, or `abandoned`), and duration. Reconcilers can time
  named steps of their work with `TraceMetadata::step`, and override a
  pass's outcome with `TraceMetadata::set_outcome`. Passes cancelled mid-way
  (for instance, on losing a leadership lease) are reported as abandoned.
* `PrometheusMetrics`, behind the new `prometheus` feature, a
  `ReconcileObserver` exporting `<namespace>_reconciliations_total`,
  `<namespace>_reconciliation_duration_seconds`,
  `<namespace>_reconciliation_steps_total`, and
  `<namespace>_reconciliation_step_duration_seconds`, with histogram
  buckets from 10ms to 30s by default.
* Kubernetes events, in the new `events` module. `EventRecorder` publishes
  `events.k8s.io/v1` events on behalf of one controller, aggregating
  identical repeats into a single event's series, truncating overlong
  notes, and giving up after a timeout (5 seconds by default).
  `Controller::with_event_recorder` publishes an event on the resource
  whenever reconciling it fails, as determined by the new
  `Context::failure_event` hook, and enables `TraceMetadata::publish_event`
  for reconcilers' own events.
* The `conditions` module, for maintaining a resource's standard
  `status.conditions`. `conditions::set` applies a `DesiredCondition`,
  keeping `lastTransitionTime` unless the condition's status changes, and
  reports whether anything changed (and so whether the status needs
  writing). `conditions::find_observed` ignores conditions determined from
  an older generation of the resource.
* `Controller::with_name`, naming a controller in its metrics and tracing
  span.
* `Error` is now exported, along with `Error::display_chain`, so that
  `Context::error_action` can be overridden outside this crate.

### Changed

* The `controller` field of the `reconcile` tracing span now defaults to the
  resource kind for contexts without a finalizer, rather than being empty.
* The `reconcile` tracing span has a new `outcome` field.

## [0.12.0] - 2026-07-22

### Added

* `LeaderElection`, providing lease-based leader election so that multiple
  replicas of a controller can run with only one reconciling at a time.
  `LeaderElection::with_lease` runs an arbitrary future (for instance, one
  or several `Controller::run` futures) while holding the lease.
  `LeaderElection::release` allows handing leadership over immediately
  during graceful shutdown.

### Changed

* `k8s-openapi` is now a regular dependency, with no version feature
  enabled; the final binary crate is responsible for enabling one.
* Update the k8s-openapi version feature used in CI and tests to `v1_34`.

## [0.3.2] - 2024-07-23

### Changed

* Upgrade `kube` and `kube-runtime` to `0.92.1`
* Upgrade `k8s-openapi` to `0.22.0`.

## [0.3.1] - 2024-06-06

### Changed

* Update k8s-openapi feature to `v1_28`.

## [0.3.0] - 2024-03-01

### Changed

* Tweaked the API for configuring concurrency a bit.

## [0.2.1] - 2024-02-29

### Changed

 * Allow for configurable max reconciliation concurrency.

## [0.2.0] - 2023-08-09

### Changed

* Upgrade `kube` and `kube-runtime` to `0.85`
    * This includes an interface change that replaces `ListParams` with `watcher::Config`, described here: https://github.com/MaterializeInc/kube-rs/blob/master/CHANGELOG.md#listwatch-changes
    * The `namespaced`, `namespaced_all`, and `cluster` methods have been updated correspondingly; this will require an update if you are using them.
* Upgrade `k8s-openapi` to `0.19` with `v1_25` enabled

## [0.1.1] - 2023-07-14

### Fixed

* A few documentation issues

## [0.1.0] - 2023-07-10

### Added

* Initial release
