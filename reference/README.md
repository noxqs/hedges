# Reference — original Python app (read-only)

`python-original/` is a copy of the original Python + GTK 3 `fence_app` package
that this project was ported from. It is **not** part of the Rust build — it's
kept here purely as the behavioural source of truth so you can compare the Rust
implementation against the original when tuning.

Each Rust module in `../src/` maps to the same-named Python file here:

| Rust (`../src/`) | Python (`python-original/`) |
|---|---|
| `app.rs` | `app.py` |
| `fence_window.rs` | `fence_window.py` |
| `settings_window.rs` | `settings_window.py` |
| `config.rs` | `config.py` |
| `colors.rs` | `colors.py` |
| `desktop_apps.rs` | `desktop_apps.py` |

`python-original-README.md` is the original project's README.
