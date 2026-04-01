# Contributing to Ozma

Thank you for contributing to ozma. This document covers the process and guidelines.

## Developer Certificate of Origin (DCO)

All contributions must include a DCO sign-off. This certifies that you have the
right to submit the code and agree to the AGPL-3.0 license.

Add this line to every commit message:

```
Signed-off-by: Your Name <your.email@example.com>
```

Use `git commit -s` to add it automatically.

The DCO text (https://developercertificate.org/):

> By making a contribution to this project, I certify that:
>
> (a) The contribution was created in whole or in part by me and I have
> the right to submit it under the open source license indicated in the file; or
>
> (b) The contribution is based upon previous work that, to the best of my
> knowledge, is covered under an appropriate open source license and I have
> the right under that license to submit that work with modifications,
> whether created in whole or in part by me, under the same open source
> license (unless I am permitted to submit under a different license),
> as indicated in the file; or
>
> (c) The contribution was provided directly to me by some other person who
> certified (a), (b) or (c) and I have not modified it.
>
> (d) I understand and agree that this project and the contribution are public
> and that a record of the contribution (including all personal information
> I submit with it, including my sign-off) is maintained indefinitely and may
> be redistributed consistent with this project or the open source license(s)
> involved.

## License

By contributing, you agree that your contributions will be licensed under:

- **AGPL-3.0** for all platform code (controller, node, softnode, agent, firmware, web UI)
- **CC-BY-4.0** for documentation
- **CC-BY-SA-4.0** for phone mic compensation data (if contributing to ozma-mic-db)

Note: hardware designs (PCBs, enclosures) are proprietary and not open for contribution.

See `COPYING` for the full license text including the plugin exception.

## How to Contribute

### Reporting Bugs

Open a GitHub issue using the bug report template. Include:

- What you expected to happen
- What actually happened
- Steps to reproduce
- Your environment (OS, Python version, hardware)
- Relevant logs (controller, node, or browser console)

### Suggesting Features

Open a GitHub issue using the feature request template. Describe the use case,
not just the solution.

### Submitting Code

1. Fork the repository
2. Create a branch from `main` (`git checkout -b feature/your-feature`)
3. Make your changes
4. Run the tests (`python -m pytest tests/`)
5. Commit with DCO sign-off (`git commit -s -m "Add feature X"`)
6. Push and open a Pull Request against `main`

### Code Style

Follow the existing conventions documented in `CLAUDE.md`:

- Python 3.11+ — use `X | Y` union types, `match`, walrus operator
- `asyncio` throughout — no threading except where forced by library
- All long-running work as named `asyncio.create_task(..., name="descriptor")`
- Logging: `log = logging.getLogger("ozma.component")`
- No external state — everything flows through `AppState` and event queue
- Subprocesses via `asyncio.create_subprocess_exec` in async paths
- Don't add fallback handling for impossible states
- Keep it simple — three similar lines are better than a premature abstraction

### Tests

- Run existing tests before submitting: `python -m pytest tests/`
- Add tests for new functionality where practical
- E2E tests require the running stack (`demo/start_vms.sh`)

### Plugins

Third-party plugins can be any license (see the plugin exception in `COPYING`).
Plugin development guide: see `controller/plugin_api.py` for the API.

## Code of Conduct

This project follows the Contributor Covenant Code of Conduct.
See `CODE_OF_CONDUCT.md`.

## Questions

Open a GitHub Discussion or ask in the issue tracker.
