//! Copies vole-kit's templates into `templates/kit/` (see vole-kit-build), so
//! Wren's pages can extend the kit's layouts.

fn main() {
    vole_kit_build::templates();
}
