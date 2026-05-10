<p align="center">
  <img src="docs/logo.svg" width="200" alt="tau logo">
</p>


# ([dpc's](#other-agents-named-tau)) Tau coding agent

> Tau is like [Pi][pi], but twice as much.

Tau is a minimal Unix-first coding agent for people who want local control, simple process boundaries, and tooling that fits naturally into a command-line environment.

Tau runs its main components as standalone POSIX processes and connects them over stdio and Unix sockets.

Components include:

* UI
* Harness
* LLM provider integration
* Extensions

This architecture has important benefits:

* Starting a component is just running a process — supervise, sandbox, restart, or swap it for anything else that speaks the protocol.
* Components can be system-provided, which pairs well with technologies like NixOS.
* Components can be sandboxed individually using tools like bubblewrap, Docker, jails, or Landlock, according to their actual needs.
* Components can be implemented in any programming language.
* It avoids bringing in web technology where it does not belong.

[pi]: https://shittycodingagent.ai/


## Features

See [FEATURES.md](FEATURES.md) for a tour of the major features.


## Status: call for testing

Tau is still young, but the core functionality is working and it is ready for testing.

Expect rough edges. If you try it, please report bugs, usability problems, and missing pieces.

[![asciicast](https://asciinema.org/a/973826.svg)](https://asciinema.org/a/973826)

## Installing

### via Nix

Tau exposes a Nix flake and can be started with `nix run github:dpc/tau`.

You can also import it as a flake input.

### via `cargo`

Tau is a Rust project and can be installed directly from Git:

```sh
cargo install --git https://github.com/dpc/tau tau
```

### via other means

Official packaging will come later — request a format or upvote existing requests on [GitHub Discussions](https://github.com/dpc/tau/discussions) to help prioritize.


## Configuration

Use `tau init` to generate config files.

You can edit `models.json5` to configure local models, and use `tau provider add` to log in to hosted providers.

By default, `tau` starts the harness daemon and the CLI UI.

To explore other entry points, run `tau -h`.


## Contributing & Contact

* [GitHub Discussions](https://github.com/dpc/tau/discussions) — questions, ideas, general conversation
* [I don't want your PRs anymore](https://dpc.pw/posts/i-dont-want-your-prs-anymore/) — I do not accept pull requests
* [`#support:dpc.pw` Matrix channel](https://matrix.to/#/#support:dpc.pw)
* [Rostra p2p social network profile](https://rostra.me/profile/rse1okfyp4yj75i6riwbz86mpmbgna3f7qr66aj1njceqoigjabegy)


## License

[Mozilla Public License 2.0](LICENSE)

## Other agents named Tau

Because the author is not very original and forgot to do prior research,
and Tau is just such a good name, there are other coding harnesses called "Tau", like:

* https://github.com/tau-agent/tau - CLI, in Rust, probably quite similar
* https://taulepton.com/
* https://github.com/AbdoKnbGit/tau
* https://alexledger.substack.com/p/tau-the-self-modifying-browser-based

Get used to it, I guess. Now that we can all be so productive,
we'll have forks and personal re-implementations of everything,
with conflicting names.

When you want to be specific, you can call this one "dpc's Tau coding agent".


## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)

Because of its nature, this project is more AI-assisted than most of my other work.
