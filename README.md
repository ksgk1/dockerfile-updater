# Dockerfile Updater (dfu)

## Made by humans

![brainmade](./assets/white-logo.png)

## tl;dr

The tool allows to get a new version for a given dockerfile. There are 5 different strategies: Update to next minor version, update to latest minor version, update to next major version and update to lastest major version, or latest available (a combination of the latest minor and major). Major updates can be ommited for given images.

## Examples

```bash
dfu input mcr.microsoft.com/dotnet/aspnet:9.0.0 --strat next-patch -q
mcr.microsoft.com/dotnet/aspnet:9.0.1

dfu overview node:22.6.0-bookworm-slim -q
Results for:    node:22.6.0-bookworm-slim
next minor:     node:22.7-bookworm-slim
latest minor:   node:22.22.0-bookworm-slim
next major:     node:23.0-bookworm-slim
latest major:   node:25.6.1-bookworm-slim
```

## Features

* The tool tries to keep the variant (e.g. alpine) in place and respects this during the update.
* The tool operates on semver tags only. Major and minor have to be already given. See example dockerfile.
* Cache files will be saved in a platform specific temp folder, to reduce unncessary traffic (cache will be updated after one hour).
* Support for Dockerhub and Microsoft Container Registry (MCR)
* Quiet-mode only prints the result, in case the output need to be captured.
* Updating entire dockerfile(s) via file input. Dry-run can be used for a preview.
* Help available via: `dockerimage-updater --help`.
* Self-update (downloads new version next to the current binary)
* Storing and loading configurations for easier reuse.

## Notes

* Filtering by architecture (e.g. "amd64") will be done on the initial fetch, when creating the cache. The cache file does not contain information about the architecture, and may lead to incorrect results. This should only be used when working with non-amd64 images, where the common tags might not exist.
