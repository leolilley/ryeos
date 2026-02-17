# Publishing RYE Packages to PyPI

Step-by-step guide for publishing `lilux`, `rye-core`, and `rye-mcp` to PyPI.

---

## 1. Package Names on PyPI

| PyPI Name  | Source Dir  | Status                     | Notes                                                  |
| ---------- | ----------- | -------------------------- | ------------------------------------------------------ |
| `lilux`    | `lilux/`    | Available                  | Microkernel, zero deps                                 |
| `rye-core` | `rye/`      | Needs rename from `rye-os` | Core engine + ALL `.ai/` data (directives, tools, knowledge) |
| `rye-mcp`  | `rye-mcp/`  | Ready                      | MCP server code only, no `.ai/` data                   |

> **Why not `rye`?** The name `rye` is taken on PyPI. `rye-core` is the base engine.
>
> **What is `rye-os`?** It's a **bundle name**, not a PyPI package. The `rye-mcp` package registers a `rye-os` entry point in the `rye.bundles` group that aggregates rye-core's `.ai/` data via `importlib.util.find_spec("rye")`. There is no `rye-os` pip-installable package.

---

## 2. Pre-Publication Checklist

- [ ] Rename `rye/pyproject.toml` `name` from `rye-os` to `rye-core`
- [ ] Update `rye-mcp/pyproject.toml` dependency from `rye-os` to `rye-core`
- [ ] Add proper metadata to all `pyproject.toml` files (author, license, urls, classifiers, `requires-python`)
- [ ] Add `README.md` to each package directory (PyPI renders this as the package description)
- [ ] Verify `.ai/` data files are included in `rye-core` wheel (`force-include` in hatch config — already done in `rye/pyproject.toml`)
- [ ] Confirm `rye-mcp` has **no** `.ai/` directory or `force-include` — all data lives in `rye-core`
- [ ] Test `python -m build` + `twine check dist/*` for each package
- [ ] Publish to TestPyPI and test installs before real PyPI

---

## 3. Required pyproject.toml Metadata

### lilux

```toml
[build-system]
requires = ["flit_core >=3.2,<4"]
build-backend = "flit_core.buildapi"

[project]
name = "lilux"
version = "0.1.0"
description = "Lilux - Microkernel for RYE OS"
readme = "README.md"
license = {text = "MIT"}
requires-python = ">=3.11"
authors = [{name = "Leo Lilley", email = "leo@example.com"}]
classifiers = [
    "Development Status :: 3 - Alpha",
    "Intended Audience :: Developers",
    "License :: OSI Approved :: MIT License",
    "Programming Language :: Python :: 3",
    "Programming Language :: Python :: 3.11",
    "Programming Language :: Python :: 3.12",
    "Programming Language :: Python :: 3.13",
    "Topic :: Software Development :: Libraries",
]
dependencies = []

[project.optional-dependencies]
dev = [
    "pytest>=7.0",
    "pytest-asyncio>=0.21.0",
]

[project.urls]
Homepage = "https://github.com/leolilley/rye-os"
Repository = "https://github.com/leolilley/rye-os"
Documentation = "https://github.com/leolilley/rye-os/tree/main/new_docs"
```

### rye-core

```toml
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[project]
name = "rye-core"
version = "0.1.0"
description = "RYE Core - AI operating system engine running on Lilux microkernel"
readme = "README.md"
license = {text = "MIT"}
requires-python = ">=3.11"
authors = [{name = "Leo Lilley", email = "leo@example.com"}]
classifiers = [
    "Development Status :: 3 - Alpha",
    "Intended Audience :: Developers",
    "License :: OSI Approved :: MIT License",
    "Programming Language :: Python :: 3",
    "Programming Language :: Python :: 3.11",
    "Programming Language :: Python :: 3.12",
    "Programming Language :: Python :: 3.13",
    "Topic :: Software Development :: Libraries",
]
dependencies = [
    "lilux",
    "pyyaml",
    "cryptography",
    "packaging>=21.0",
]

[project.optional-dependencies]
dev = [
    "pytest>=7.0",
    "pytest-asyncio>=0.21.0",
    "pytest-cov>=4.0",
]

[project.entry-points."rye.bundles"]
rye-core = "rye.bundle_entrypoints:get_rye_core_bundle"

[project.urls]
Homepage = "https://github.com/leolilley/rye-os"
Repository = "https://github.com/leolilley/rye-os"
Documentation = "https://github.com/leolilley/rye-os/tree/main/new_docs"

[tool.hatch.build.targets.wheel]
packages = ["rye"]

[tool.hatch.build]
exclude = [".venv", "__pycache__", "*.pyc"]

[tool.hatch.build.targets.wheel.force-include]
"rye/.ai" = "rye/.ai"
```

### rye-mcp

```toml
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[project]
name = "rye-mcp"
version = "0.1.0"
description = "MCP server transport for RYE OS"
readme = "README.md"
license = {text = "MIT"}
requires-python = ">=3.11"
authors = [{name = "Leo Lilley", email = "leo@example.com"}]
classifiers = [
    "Development Status :: 3 - Alpha",
    "Intended Audience :: Developers",
    "License :: OSI Approved :: MIT License",
    "Programming Language :: Python :: 3",
    "Programming Language :: Python :: 3.11",
    "Programming Language :: Python :: 3.12",
    "Programming Language :: Python :: 3.13",
    "Topic :: Software Development :: Libraries",
]
dependencies = [
    "rye-core",
    "mcp",
]

[project.optional-dependencies]
dev = [
    "pytest>=7.0",
    "pytest-asyncio>=0.21.0",
]

[project.scripts]
rye-mcp = "rye_mcp.server:main"

[project.entry-points."rye.bundles"]
rye-os = "rye_mcp.bundle_entrypoints:get_rye_os_bundle"

[project.urls]
Homepage = "https://github.com/leolilley/rye-os"
Repository = "https://github.com/leolilley/rye-os"
Documentation = "https://github.com/leolilley/rye-os/tree/main/new_docs"

[tool.hatch.build.targets.wheel]
packages = ["rye_mcp"]
```

> **Note:** `rye-mcp` has **no** `force-include` and **no** `.ai/` directory. All `.ai/` data is shipped inside `rye-core`. The `rye-mcp` bundle entrypoint locates it at runtime via `importlib.util.find_spec("rye")`.

---

## 4. Build & Publish Steps

### Install build tools

```bash
pip install build twine
```

### Build all packages

```bash
# Build lilux
cd lilux
rm -rf dist/
python -m build
twine check dist/*

# Build rye-core
cd ../rye
rm -rf dist/
python -m build
twine check dist/*

# Build rye-mcp
cd ../rye-mcp
rm -rf dist/
python -m build
twine check dist/*
```

### Upload to TestPyPI first

```bash
# Upload each package (order matters — dependencies first)
cd ../lilux
twine upload --repository testpypi dist/*

cd ../rye
twine upload --repository testpypi dist/*

cd ../rye-mcp
twine upload --repository testpypi dist/*
```

### Test install from TestPyPI

```bash
pip install \
  --index-url https://test.pypi.org/simple/ \
  --extra-index-url https://pypi.org/simple/ \
  rye-mcp
```

The `--extra-index-url` is needed because transitive deps (pyyaml, cryptography, mcp, etc.) are on real PyPI. Installing `rye-mcp` pulls in `rye-core` and `lilux` automatically.

### Upload to real PyPI

```bash
# Same order — dependencies first
cd lilux && twine upload dist/*
cd ../rye && twine upload dist/*
cd ../rye-mcp && twine upload dist/*
```

### Configure credentials

Create `~/.pypirc` or use environment variables:

```ini
[distutils]
index-servers =
    pypi
    testpypi

[pypi]
username = __token__
password = pypi-XXXXXXXXXXXX

[testpypi]
repository = https://test.pypi.org/legacy/
username = __token__
password = pypi-XXXXXXXXXXXX
```

Or use environment variables:

```bash
export TWINE_USERNAME=__token__
export TWINE_PASSWORD=pypi-XXXXXXXXXXXX
```

---

## 5. Versioning Strategy

- All packages start at **0.1.0**
- Use **semantic versioning** (`MAJOR.MINOR.PATCH`)
- `lilux`, `rye-core`, and `rye-mcp` version **independently**

### When to bump

| Change type                   | Bump  | Example        |
| ----------------------------- | ----- | -------------- |
| Bug fix, no API change        | PATCH | 0.1.0 → 0.1.1 |
| New feature, backwards compat | MINOR | 0.1.1 → 0.2.0 |
| Breaking API change           | MAJOR | 0.2.0 → 1.0.0 |

### Release order

Always publish in dependency order: `lilux` → `rye-core` → `rye-mcp`.

---

## 6. Install Commands for Users

```bash
# Just the microkernel
pip install lilux

# Core engine + all .ai/ data (includes lilux)
pip install rye-core

# Everything — MCP server + core engine + all data (includes rye-core + lilux)
pip install rye-mcp
```

---

## 7. Development Install (from source)

```bash
git clone https://github.com/leolilley/rye-os.git
cd rye-os

pip install -e lilux
pip install -e rye          # installs as rye-core
pip install -e rye-mcp      # installs as rye-mcp with CLI entry point
```

After editable install, the `rye-mcp` CLI command is available:

```bash
rye-mcp  # starts the MCP server
```

---

## 8. Rename Steps (Before First Publish)

These are the exact changes needed:

### Step 1: Rename rye-os → rye-core in `rye/pyproject.toml`

```diff
- name = "rye-os"
+ name = "rye-core"
```

### Step 2: Update dependency in `rye-mcp/pyproject.toml`

```diff
  dependencies = [
-     "rye-os",
+     "rye-core",
      "mcp",
  ]
```

### Step 3: Add metadata to all pyproject.toml files

Add `readme`, `license`, `requires-python`, `classifiers`, and `urls` to:

- `lilux/pyproject.toml`
- `rye/pyproject.toml`
- `rye-mcp/pyproject.toml`

See [Section 3](#3-required-pyprojecttoml-metadata) for the full content.

### Step 4: Add README.md to each package

```bash
# Ensure each package dir has its own README
ls lilux/README.md rye/README.md rye-mcp/README.md
```

### Step 5: Build, check, test, publish

Follow [Section 4](#4-build--publish-steps).
