# Changelog

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
