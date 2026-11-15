# Contributing

Thanks for your interest. Sitewright is young; issues and small, focused pull requests are
welcome.

1. Run `./scripts/dev-setup.sh` once (installs Rust and dev tools into your home directory).
2. Create a branch, make your change, add tests.
3. Run the checks listed in [AGENTS.md](AGENTS.md#build-and-test).
4. Use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages.
5. Open a pull request. CI must be green before merge.

Design decisions are recorded as ADRs in [docs/adr/](docs/adr/); a change that reverses one
needs a new ADR. Please report security issues privately, as described in
[SECURITY.md](SECURITY.md).
