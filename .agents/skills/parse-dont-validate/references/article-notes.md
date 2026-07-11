# Article Notes

Source: Alexis King, "Parse, don't validate" (2019-11-05), https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/

## Core Learning

- Validation checks a weak value and returns no useful value, often `Result<()>`.
- Parsing consumes a weak value and returns a stronger value that preserves the learned fact in the type system.
- Strengthen argument types instead of weakening return types when a precondition can be expressed in data.
- Push proof upward to the boundary where the data is created or received, but no further.
- Avoid shotgun parsing: do not mix input checks through processing code after acting on the input.
- Abstract newtypes with private constructors are acceptable when Rust cannot express the invariant directly, such as numeric ranges.
- Functions returning `m ()` or `Result<()>` deserve suspicion when their main purpose is to reject invalid input.

## Rust Translation

Prefer:

```rust
pub fn parse(input: &str) -> Result<EphFile>;
pub fn start_services(config: &EphFile) -> Result<()>;
```

Avoid:

```rust
pub fn validate(input: &str) -> Result<()>;
pub fn start_services(raw: &str) -> Result<()>;
```

The first shape makes parsing mandatory for lifecycle work. The second shape allows a caller to forget validation and still compile.

## eph examples

- `.eph` text parses into `EphFile`, whose services always have exactly one source and whose role graph is complete and acyclic.
- Port declarations parse into `PortMapping`, so auto-allocated ports cannot carry an invalid fixed number and Compose mappings retain both alias and target service.
- `command=` parses into `CommandOverride` before startup, so runtime code receives argv rather than reparsing command text.
- Persisted state may deserialize through a compatibility representation, but lifecycle code should consume the typed `Backend` variants.
