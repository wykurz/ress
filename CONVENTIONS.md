- For functions and types prefer fully qualified names, e.g. `std::net::SocketAddr`.
- Import macros/traits used in macros, e.g. `use serde::{Deserialize, Serialize};`.
- Prefer no empty lines inside functions or type definitions.
- When importing crates specify only major and minor version (e.g. `1.41`, not `1.41.0`).
- Don't start `//` comments with a capital letter; use a period only to separate sentences.
- Doc comments (`///`, `//!`) start with a capitalized sentence and read naturally.

## Testing
- Name tests after observed behavior (e.g. `returns_first_screen`, `chops_long_lines`).
- Compact structure: setup, execute, assert, with minimal empty lines.
- Comments explain "why", not "what".
