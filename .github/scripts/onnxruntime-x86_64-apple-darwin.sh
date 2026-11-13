#!/usr/bin/env bash
# ONNX Runtime for Intel macOS (x86_64-apple-darwin), built from Microsoft's source as one static library, for
# ampup-memory to link: the ort crate downloads a prebuilt ONNX Runtime for every other release platform, and has none
# for Intel Macs (nor does Microsoft, after 1.23). The version is licenses/onnxruntime/VERSION, the one ort downloads
# everywhere else (third-party-notices.sh checks that), so THIRD-PARTY-NOTICES.txt describes this build too.
#
#   .github/scripts/onnxruntime-x86_64-apple-darwin.sh <out-dir>
#   ORT_LIB_LOCATION=<out-dir> cargo build --release --target x86_64-apple-darwin --bin ampup-memory
#
# Writes <out-dir>/libonnxruntime.a (and VERSION) and nothing else; returns at once when <out-dir> already holds this
# version, so a CI cache of <out-dir> skips the build. Runs on macOS, Apple Silicon (cross-compiling) or Intel, with
# Xcode's command line tools, cmake, python3, git and the network (GitHub, for the source and its dependencies). Takes
# ~4 minutes on an M3 Pro (12 cores; ~35 CPU-minutes), several times that on a 3-core CI runner.
set -euo pipefail

out=${1:?usage: .github/scripts/onnxruntime-x86_64-apple-darwin.sh <out-dir>}
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(cat "$root/licenses/onnxruntime/VERSION")
# The oldest macOS the build runs on: what ort's prebuilt Apple Silicon library is built for.
deploy_target=13.4

if [ -f "$out/libonnxruntime.a" ] && [ "$(cat "$out/VERSION" 2>/dev/null)" = "$version" ]; then
  echo "$out already holds ONNX Runtime $version" >&2
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --branch "v$version" https://github.com/microsoft/onnxruntime "$work/src"
# No tests, no Python. --build_shared_lib only so that every static library the runtime needs gets built (re2, for
# one, is built only for a target that links it); the dylib itself is left behind. Its dependencies (abseil,
# protobuf, onnx, re2, ...) are always the archives cmake/deps.txt pins, never what Homebrew installed (another
# version, and arm64 on an Apple Silicon Mac): FETCHCONTENT_TRY_FIND_PACKAGE_MODE and CMAKE_IGNORE_PREFIX_PATH.
# CMAKE_POLICY_VERSION_MINIMUM: some of them declare a cmake older than 3.5, which cmake 4 refuses.
python3 "$work/src/tools/ci_build/build.py" \
  --build_dir "$work/build" --config Release --update --build --build_shared_lib --parallel \
  --skip_submodule_sync --skip_tests --compile_no_warning_as_error \
  --osx_arch x86_64 --apple_deploy_target "$deploy_target" \
  --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF FETCHCONTENT_TRY_FIND_PACKAGE_MODE=NEVER \
  'CMAKE_IGNORE_PREFIX_PATH=/opt/homebrew;/usr/local' CMAKE_POLICY_VERSION_MINIMUM=3.5 >&2

# One archive of every static library the build made, ONNX Runtime's and its dependencies', as ort's prebuilt ones
# are (ort links a lone libonnxruntime.a as it is). Not the full protobuf (ONNX Runtime links the lite one), nor
# test or benchmark libraries, were any built.
release=$work/build/Release
libs=$(
  find "$release" -name '*.a' ! -name 'libprotobuf.a' ! -name 'libprotoc.a' ! -name 'libgtest*.a' \
    ! -name 'libgmock*.a' ! -name 'libbenchmark*.a' | sort
)
mkdir -p "$out"
# shellcheck disable=SC2086 # $libs is a list of paths without spaces (the mktemp directory's)
libtool -static -no_warning_for_no_symbols -o "$out/libonnxruntime.a" $libs 2> >(grep -v 'duplicate member name' >&2)
echo "$version" > "$out/VERSION"
echo "wrote $out/libonnxruntime.a (ONNX Runtime $version, x86_64, macOS $deploy_target+, $(du -h "$out/libonnxruntime.a" | cut -f1))" >&2
