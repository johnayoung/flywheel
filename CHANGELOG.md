# Changelog

## [0.5.0](https://github.com/johnayoung/flywheel/compare/v0.4.2...v0.5.0) (2026-04-17)


### ⚠ BREAKING CHANGES

* strip orchestration layer in preparation for redesign

### Refactoring

* strip orchestration layer in preparation for redesign ([81343e7](https://github.com/johnayoung/flywheel/commit/81343e795808636b057a93f97ebcc38d9c244a01))

## [0.4.2](https://github.com/johnayoung/flywheel/compare/v0.4.1...v0.4.2) (2026-04-16)


### Bug Fixes

* **validate:** auto-fix commit message type instead of failing validation ([85c81b5](https://github.com/johnayoung/flywheel/commit/85c81b5f16e7547694d3d2a809b9793fd3378c94))

## [0.4.1](https://github.com/johnayoung/flywheel/compare/v0.4.0...v0.4.1) (2026-04-16)


### Bug Fixes

* **claudecode:** drain streaming scanner before cmd.Wait ([aed3cfe](https://github.com/johnayoung/flywheel/commit/aed3cfe5a6c51012a136898898fc5eadd6b787b5))


### Refactoring

* **validate:** mechanical-only gate; surface attempt failure detail ([c0ca2e2](https://github.com/johnayoung/flywheel/commit/c0ca2e240ab9faba7f6d7812d7e36b983ad8ca82))

## [0.4.0](https://github.com/johnayoung/flywheel/compare/v0.3.0...v0.4.0) (2026-04-15)


### Features

* **engine:** self-healing orchestration and defensive worktree lifecycle ([e47896a](https://github.com/johnayoung/flywheel/commit/e47896a7bf549fb3cf50d120eec4d22fc9b83e88))

## [0.3.0](https://github.com/johnayoung/flywheel/compare/v0.2.0...v0.3.0) (2026-04-15)


### Features

* **cli:** preflight git repo and base_ref before init/run ([7d52bb3](https://github.com/johnayoung/flywheel/commit/7d52bb3857d6fc24817fbe85d53de64aac0e08d4))


### Bug Fixes

* **lifecycle:** stop double-bumping Version on Transition+UpdateLifecycle ([f98c2cf](https://github.com/johnayoung/flywheel/commit/f98c2cfe94d6004e49298519ca2c54635ae18a5b))
