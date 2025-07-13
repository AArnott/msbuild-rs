# Renovate Configuration for MSBuild-RS

This repository uses [Renovate Bot](https://github.com/renovatebot/renovate) for automated dependency management. Renovate provides more advanced features and better customization compared to Dependabot.

## Configuration Overview

The Renovate configuration is defined in [`renovate.json`](./renovate.json) and provides:

### 🔄 **Automated Updates**
- **Rust dependencies** (Cargo.toml)
- **GitHub Actions** (workflow files)
- **Docker images** (Dockerfile)
- **Development tools** (cargo-* utilities)

### 📅 **Scheduling**
- **Regular updates**: Monday mornings before 6 AM UTC
- **Security updates**: Immediate (any time)
- **Lock file maintenance**: Weekly on Mondays

### 🏷️ **Grouping Strategy**
- **Rust dependencies**: Grouped by type (dev-tools, test dependencies, etc.)
- **Security updates**: High priority, immediate processing
- **Major updates**: Separate PRs with extended stability period

### 🔒 **Security Features**
- **Vulnerability alerts**: Immediate updates for security issues
- **Stability period**: 3-day wait for regular updates, 7 days for major updates
- **Release confidence**: Adoption and compatibility metrics included

## Update Categories

### Rust Dependencies
```json
{
  "groupName": "Rust dependencies",
  "semanticCommitType": "deps",
  "semanticCommitScope": "rust",
  "stabilityDays": 3
}
```

### Security Updates
```json
{
  "groupName": "Rust security updates",
  "prPriority": 10,
  "minimumReleaseAge": "0 days",
  "schedule": ["at any time"]
}
```

### GitHub Actions
```json
{
  "groupName": "GitHub Actions",
  "semanticCommitType": "ci",
  "semanticCommitScope": "actions",
  "pinDigests": true
}
```

### Docker Images
```json
{
  "semanticCommitType": "docker",
  "semanticCommitScope": "base",
  "minimumReleaseAge": "3 days"
}
```

## Special Handling

### Rust Docker Image
The Rust Docker image in the devcontainer gets special treatment:
- **Extended stability period**: 7 days
- **Custom regex manager**: Tracks version in Dockerfile comments
- **Higher priority**: Priority 8 for important updates

### Development Tools
Common development tools are grouped together:
- cargo-* utilities
- serde ecosystem
- clap argument parser
- logging libraries

## Commit Message Format

Renovate uses semantic commit messages:
```
deps(rust): Update serde to 1.0.195
ci(actions): Update actions/checkout to v4
docker(rust): Update rust to 1.75.1
```

## PR Templates

Each Renovate PR includes:
- **Dependency comparison table** with age, adoption, and confidence metrics
- **Release notes** for updated packages
- **Configuration summary** showing schedule and automerge settings
- **Merge confidence badges** for safety assessment

## Dashboard

Renovate provides a dependency dashboard issue that shows:
- **Pending updates**: What's waiting to be processed
- **Rate limiting**: Current status and limits
- **Configuration errors**: Any issues with the setup
- **Manual triggers**: Force updates when needed

## Manual Control

### Force Updates
Comment on the dependency dashboard issue:
```
@renovate rebase
```

### Skip Updates
Close the PR or add to ignored list in config.

### Emergency Updates
Security updates bypass normal scheduling and stability periods.

## Integration with CI

Renovate PRs trigger the full CI pipeline:
- **Multi-platform testing**: Linux, Windows, macOS
- **Security scanning**: cargo-audit and cargo-deny
- **Code quality**: Format, lint, and documentation checks
- **Integration testing**: Demo mode and sample projects

## Benefits Over Dependabot

1. **Better grouping**: Related dependencies updated together
2. **Release confidence**: Adoption and compatibility metrics
3. **Flexible scheduling**: Time-based and condition-based triggers
4. **Custom managers**: Handle non-standard dependency files
5. **Rich PR content**: Release notes, changelogs, and metrics
6. **Advanced configuration**: Fine-grained control over update behavior

## Monitoring

Check these regularly:
- **Dependency Dashboard**: GitHub issue for Renovate status
- **Security Alerts**: GitHub security tab for vulnerabilities
- **CI Status**: Ensure Renovate PRs pass all checks
- **Release Notes**: Review changes in dependency updates

The configuration is designed to balance keeping dependencies current with maintaining stability and avoiding noise from unnecessary updates.
