# Host adapters

Production executables installed by an external host administrator live here.
They adapt host-owned lifecycle or isolation facilities into protected RyeOS
runtime inputs. They are not RyeOS Tool binaries and are not launched as
Tools by the RyeOS execution engine.

Host adapters may compose Lillux kernel APIs with narrow RyeOS binding formats,
but RyeOS must not use them to acquire or manage external host authority.
