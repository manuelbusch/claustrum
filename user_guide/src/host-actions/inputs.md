# Inputs

Inputs are the only part of an action Claude controls. Each is declared as an
`[[actions.action.input]]` block below its action and fills the `{name}` placeholders in
`command` (and `env`). They are passed positionally in declaration order
(`host test-crate claustrum-cli`) or by name (`host test-crate crate=claustrum-cli`).

## Input kinds

| `kind` | Accepts | Options |
| --- | --- | --- |
| `pattern` | values matching the anchored regex | `pattern` |
| `choices` | one of the listed values | `choices` |
| `path` | a path that resolves inside the workspace, also through symlinks | `must_exist` |
| `integer` | an integer | `min`, `max` |

`kind` can be omitted when `pattern` or `choices` is set.

```toml
[[actions.action.input]]
name = "profile"
choices = ["dev", "release"]
default = "dev"

[[actions.action.input]]
name = "file"
kind = "path"
must_exist = true

[[actions.action.input]]
name = "count"
kind = "integer"
min = 1
max = 200
default = "20"
```

## Rules for every value

Every value is limited to `max_len` bytes (default 256), may not contain control characters
or invisible Unicode formatting characters (bidi overrides and the like) and may not start
with `-` unless `allow_leading_dash = true`, so that an input can never turn into an option
of the program. `default` makes an input optional. An action without inputs refuses any
argument.

A value that fails validation refuses the whole action with exit code 2 and the reason;
nothing is started.

## Choosing patterns

Keep patterns to the characters the program actually needs. A validated value is still
interpreted by the program that receives it; [Use with care](risks.md) lists the typical
traps.
