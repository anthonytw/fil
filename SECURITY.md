# Security policy

## Reporting a vulnerability

Please report security problems privately, through GitHub's private vulnerability reporting: on [github.com/anthonytw/fil](https://github.com/anthonytw/fil), open the **Security** tab and choose **Report a vulnerability**. Don't open a public issue or pull request for one.

Say what you found, the fil version (`fil --version`), the platforms involved, and how to reproduce it if you can. A report about what fil reveals about the people using it (its privacy rules are in [`DESIGN.md`](DESIGN.md) §11.1) is welcome too.

## Supported versions

Only the latest release gets security fixes. A fix ships as a new release; older releases are not patched.

## What happens next

- You get an answer in the report, first an acknowledgment and then whether the problem is confirmed.
- While a fix is made, the report stays private. You are kept up to date in it and can test the fix.
- When the release with the fix is out, the advisory is published, crediting you unless you'd rather not be named.

This is a best effort, not a service-level promise. What fil defends against, and what it does not, is in [security and privacy](docs/user/security-privacy.md).
