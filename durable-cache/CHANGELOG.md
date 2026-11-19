# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/lmalheiro/durable-cache/compare/v0.2.0...v0.2.1) - 2026-11-19

### Performance

- write a store's last body chunk in finish's handoff ([#42](https://github.com/lmalheiro/durable-cache/pull/42))

### Changed

- take the cfg gates out of durable-cache's code paths, and tidy its style ([#39](https://github.com/lmalheiro/durable-cache/pull/39))

## [0.2.0](https://github.com/lmalheiro/durable-cache/compare/v0.1.1...v0.2.0) - 2026-11-18

### Performance

- [**breaking**] serve page-cache hits on the worker, and add an option to store without fdatasync ([#33](https://github.com/lmalheiro/durable-cache/pull/33))

### Documentation

- bring the README up to date, and add Pingora references ([#38](https://github.com/lmalheiro/durable-cache/pull/38))
- mark the SysML and OMG trademarks as the OMG trademark guidelines ask ([#35](https://github.com/lmalheiro/durable-cache/pull/35))
- replace the suppressed requirements diagram with a domain overview ([#32](https://github.com/lmalheiro/durable-cache/pull/32))

## [0.1.1](https://github.com/lmalheiro/durable-cache/compare/v0.1.0...v0.1.1) - 2026-10-21

### Documentation

- make the README's links to repository files work on crates.io ([#26](https://github.com/lmalheiro/durable-cache/pull/26))

## [0.1.0](https://github.com/lmalheiro/durable-cache/releases/tag/v0.1.0) - 2026-10-21

### Added

- durable-cache, a durable file-system storage backend for Pingora


