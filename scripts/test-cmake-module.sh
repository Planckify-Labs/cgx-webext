#!/usr/bin/env bash
# Builds a tiny consumer project that add_subdirectory()s this repository and links WebExt::WebExt, in Debug and Release.
# This is how a CMake-based browser such as Ladybird consumes the library.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/CMakeLists.txt" <<CMAKE
cmake_minimum_required(VERSION 3.25)
project(consumer C CXX)
set(CMAKE_CXX_STANDARD 23)
add_subdirectory("$root" cgx-webext)
add_executable(consumer main.cpp)
target_link_libraries(consumer PRIVATE WebExt::WebExt)
CMAKE

cat > "$tmp/main.cpp" <<'CPP'
#include <webext.h>
#include <cstdio>
#include <cstring>
#include <string>

static WebExtString no_storage(void*, WebExtStr, WebExtStr) { return { nullptr, 0, 0 }; }
static void ignore_write(void*, WebExtStr, WebExtStr, WebExtStr) { }
static void reply(void* user, bool ok, WebExtStr json)
{
    *static_cast<std::string*>(user) = std::string(ok ? "ok:" : "err:") + std::string(reinterpret_cast<char const*>(json.ptr), json.len);
}

int main()
{
    WebExtHostCallbacks callbacks {};
    callbacks.read_storage = no_storage;
    callbacks.write_storage = ignore_write;
    WebExtHandle* handle = webext_create(&callbacks);
    std::string out;
    char const* call = R"({"extensionId":"x","function":"runtime.getManifest"})";
    webext_dispatch(handle, { reinterpret_cast<uint8_t const*>(call), std::strlen(call) }, reply, &out);
    std::printf("abi=%u reply=%s\n", webext_abi_version(), out.c_str());
    webext_destroy(handle);
    // No extension "x" is installed, so an error reply proves the call went through the whole stack.
    return out.rfind("err:", 0) == 0 ? 0 : 1;
}
CPP

for build_type in Debug Release; do
    echo "==> $build_type"
    cmake -S "$tmp" -B "$tmp/build-$build_type" -G Ninja -DCMAKE_BUILD_TYPE="$build_type" ${WEBEXT_CARGO_EXTRA_ARGS:+-DWEBEXT_CARGO_EXTRA_ARGS="$WEBEXT_CARGO_EXTRA_ARGS"} > /dev/null
    cmake --build "$tmp/build-$build_type" > /dev/null
    "$tmp/build-$build_type/consumer"
done
echo "OK: CMake module works in Debug and Release"
