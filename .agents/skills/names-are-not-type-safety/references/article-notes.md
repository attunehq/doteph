# Article Notes

Source: Alexis King, "Names are not type safety" (2020-11-01), https://lexi-lambda.github.io/blog/2020/11/01/names-are-not-type-safety/

## Core Learning

- A wrapper type is not automatically safer because it has a more specific name.
- Newtypes are useful when they encode an invariant, hide representation, provide a distinct instance/trait behavior, redact secrets, rearrange type parameters, or prevent a concrete misuse across a distance.
- Transparent wrappers that are routinely wrapped and unwrapped are often taxonomy, not safety.
- Encapsulation-based safety requires a small trusted module, clear invariants, and no unsafe/public trapdoors around construction.
- Correct-by-construction datatypes are stronger than wrappers that rely on discipline.
- In application code, wrapper boundaries tend to erode over time, so prefer datatypes whose structure enforces the invariant directly.

## Rust Translation

Prefer a private-field checked wrapper when there is an invariant:

```rust
pub struct CommandOverride(Vec<String>);

pub fn parse_command_override(input: &str) -> Result<CommandOverride> {
    // Parse shell words and require a non-empty executable before construction.
}
```

Avoid a public transparent wrapper that only names a role:

```rust
pub struct ServiceName(pub String);
```

If there is no invariant, use a field name, module, doc comment, or type alias:

```rust
type ServiceName = String;
```

## eph examples

- `CommandOverride` is justified because its private field contains parsed, non-empty argv. Runtime code can consume `argv()` without repeating shell-word parsing or checking for an executable.
- `PortMapping` is an enum because fixed, auto-allocated, and Compose ports carry different valid data and require different runtime behavior.
- A hypothetical public `ServiceName(String)` would not enforce eph's service-name grammar. Keep validation at the `.eph` parse boundary, or hide the field and construct it only through that parser.
- Avoid `DerefMut` on checked wrappers unless mutation cannot break the proof or the value is reparsed before reuse.
