# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/mufeedali/flatplay/compare/v0.3.0...v0.4.0) - 2025-08-23

### Added

- [**breaking**] only `flatplay` now also stops existing process

### Other

- *(deps)* bump anyhow from 1.0.98 to 1.0.99
- *(deps)* bump clap from 4.5.44 to 4.5.45
- *(deps)* bump actions/checkout from 4 to 5
- *(deps)* bump clap_complete from 4.5.55 to 4.5.57
- *(deps)* bump clap from 4.5.42 to 4.5.44
- *(deps)* bump serde_json from 1.0.141 to 1.0.142
- *(deps)* bump clap from 4.5.41 to 4.5.42

## [0.3.0](https://github.com/mufeedali/flatplay/compare/v0.2.3...v0.3.0) - 2025-08-03

### Removed

- [**breaking**] **Removed the library**: The library was too tightly coupled with the CLI and wasn't really straightforward to use anyway.

### Added

- Show some information from the selected manifest when running Flatplay.
- `rebuild` command that performs a cleanup follow'ed by a build.

## [0.2.3](https://github.com/mufeedali/flatplay/compare/v0.2.2...v0.2.3) - 2025-08-03

### Fixed

- Show error for qmake build system
- add bind mount for document portal fuse when running app ([#8](https://github.com/mufeedali/flatplay/pull/8))

## [0.2.2](https://github.com/mufeedali/flatplay/compare/v0.2.1...v0.2.2) - 2025-08-02

### Fixed

- remove unnecessary print
- make sure build init check only happens once

### Other

- re-org build dirs
- more cleanup
- re-org build_application

## [0.2.1](https://github.com/mufeedali/flatplay/compare/v0.2.0...v0.2.1) - 2025-07-26

### Added

- add bundle export feature ([#5](https://github.com/mufeedali/flatplay/pull/5))

### Fixed

- clean when manifest switch occurs
- clean not resetting state

## [0.2.0](https://github.com/mufeedali/flatplay/compare/v0.1.1...v0.2.0) - 2025-07-26

### Fixed

- auto-selection was broken in root dir

### Other

- update readme with cargo info

## [0.1.1](https://github.com/mufeedali/flatplay/compare/v0.1.0...v0.1.1) - 2025-07-26

### Fixed

- completions command manifest error

## [0.1.0](https://github.com/mufeedali/flatplay/releases/tag/v0.1.0) - 2025-07-26

Initial release. Checkout the README for more details.
