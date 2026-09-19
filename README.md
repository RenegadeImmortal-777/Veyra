# Veyra 5.0 Frontier

Veyra is a native-first programming language platform. The default `veyra` command runs the standalone Rust compiler/bytecode VM and does not require Python.

## What Veyra owns now

The native platform includes its lexer, parser, AST, bytecode compiler, VM, project system, package registry, package manager, standard services, data/science primitives, introductory ML toolkit, web server path, desktop utility GUI layer and VS Code integration.

### Native project workflow

    veyra new app
    cd app
    veyra add vyscience
    veyra add vyml
    veyra add vydata
    veyra add vyweb
    veyra check
    veyra run
    veyra test
    veyra build

### Science/data

Native primitives include vector sums/means/variance/stddev/dot/add/scale, matrix transpose/multiply, CSV read/write, table column/filter/describe and SVG plotting.

Packages:
- vyscience
- vydata
- vyplot
- vystats
- vymath
- vyalgo

### AI/ML

Native primitives include linear regression, k-NN prediction, k-means clustering, normalization, sigmoid, MSE, dense-layer inference, ReLU and softmax.

Package:
- vyml

### Web

    veyra serve public 8080
    veyra web routes.json 8080

Veyra can serve static files and declarative method/path API routes with text, HTML and JSON responses. `vyweb` provides HTML helpers.

### Desktop utility GUI

The vygui package exposes native OS-backed alerts, confirm dialogs, input/password prompts, forms, open/save dialogs, folder selection and color selection.

### Native package ecosystem

    veyra registry
    veyra pkg search science
    veyra add vyscience
    veyra packages
    veyra install
    veyra publish

Packages are vendored into projects and captured in `veyra.lock`.

## Compatibility

The older Python-backed Convergence runtime remains available as `veyra-convergence` for compatibility. It is optional and is not the default execution path.

## Verification

The native Rust suite covers the language/runtime plus JSON, regex, hashing, file I/O, CSV, science math, ML and table/plot functionality. See `STATUS-50-ALL.md` for the exact interpretation of the 50%-class milestone.
