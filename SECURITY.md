# Security

nerdfleet logs in to your machines over SSH, copies an agent there and runs it as you, so a flaw in it can matter
more than in most terminal tools. If you find one, please report it privately rather than in a public issue.

## Reporting

Use **Report a vulnerability** on the repository's
[Security tab](https://github.com/Ndles/nerdfleet/security/advisories/new). Say what you found, how to reproduce it,
and what an attacker would gain. You will get an answer within a week; a fix goes into the next release, and the
advisory is published with it, crediting you unless you would rather not.

## What is in scope

- **The agent and its deployment:** the copy to `~/.cache/nerdfleet/`, its checks before it runs, the resident
  service, and anything that lets one user's agent be run or changed by another.
- **SSH:** options nerdfleet passes, anything from the config or a host that could become an `ssh` option or a
  shell command.
- **Untrusted input:** data an agent or a terminal sends that could take over the viewer's terminal, crash it, or
  reach the clipboard or desktop notifications.

Out of scope: problems that need the attacker to already control your account on the viewer's machine, and
weaknesses in OpenSSH or your SSH configuration themselves.

## Supported versions

Only the latest release gets fixes.
