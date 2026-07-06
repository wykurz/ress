- For functions and types prefer fully qualified names, e.g. `std::net::SocketAddr`.
- Import macros/traits used in macros, e.g. `use serde::{Deserialize, Serialize};`.
- Prefer no empty lines inside functions or type definitions.
- When importing crates specify only major and minor version (e.g. `1.41`, not `1.41.0`).
- Don't capitalize `//` comments; use a period only to delimit (separate) sentences, not to end a single fragment.
- Doc comments (`///`, `//!`) are capitalized and read like proper English sentences.

## Testing
- Name tests after observed behavior (e.g. `returns_first_screen`, `chops_long_lines`).
- Compact structure: setup, execute, assert, with minimal empty lines.
- Comments explain "why", not "what".
