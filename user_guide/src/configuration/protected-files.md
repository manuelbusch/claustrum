# Protected files

## The configuration is read-only inside the sandbox

Every file Claustrum could load its configuration from is protected: `claustrum.toml` in the
workspace (even when it does not exist yet), the default search paths and the file passed
with `--config`, in every mount that contains them. The guest can read them but writing,
truncating, creating, deleting, renaming or replacing them is refused, as is renaming or
removing a directory that contains one. The check runs on the host below every tool, follows
symlinks and hard links, and compares names case-insensitively on macOS. Change the
configuration from outside the sandbox.

## Mounts stay inside their directory

Every host-backed mount is also confined to its directory. A symlink that points outside,
checked into a repository or created by a host action, is refused by every tool; links
inside the mount keep working. When a host action changes a protected file, Claustrum puts
it back without following anything the action left in its place: links are removed,
directories are moved aside as `<name>.claustrum-moved`.

## Claude Code's project settings

The same holds for Claude Code's own project settings, `.claude/settings.json` and
`.claude/settings.local.json`. Claude Code runs on the host, outside the sandbox, and executes
the hooks, status line and helper commands configured there. The guest can neither create
those files nor turn `.claude` into a link. Other files in `.claude` (agents, commands,
skills) stay writable. Their shell snippets only run through the built-in Bash tool, which
Claustrum removes. When `[sandbox] workspace` points to a directory above the project, the
settings next to the loaded `claustrum.toml` are protected as well.

With OS confinement, `claustrum serve` creates `.claude` in the workspace if it is missing.
Under bubblewrap, a settings file that does not exist yet cannot be protected by a bind mount
of its own, so `.claude` itself is bound read-only and its existing entries read/write again:
a new entry directly in `.claude` (a new `agents` directory, say) has to be created outside
the sandbox, while everything below existing entries stays writable.

On Linux without user namespaces, a few of these guarantees rest on the WASIX layer alone;
see [OS confinement](../security/confinement.md#linux-without-user-namespaces).
