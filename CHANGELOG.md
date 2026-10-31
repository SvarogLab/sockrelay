# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-31

### Added ✨

- Run under systemd only, on librelay

### Build system 🛠️

- Stop tracking the lockfile

### CI/CD ⚙️

- Check, test and release through the shared workflows (#2)

### Documentation 📚

- Keep the documentation in english only (#4)

### Miscellaneous 🧹

- Apply the repository skeleton
- Let renovate update dependencies through the shared preset

### Style 🎨

- Format with the skeleton's rustfmt configuration

### Tests ✅

- Keep child processes from holding a closed listener open (#3)
