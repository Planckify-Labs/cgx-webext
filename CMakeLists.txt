# cgx-webext: WebExtensions (Chrome/Firefox) compatibility layer.
#
# Use from a browser's build with:
#     add_subdirectory(/path/to/cgx-webext cgx-webext)
#     target_link_libraries(<your target> PRIVATE WebExt::WebExt)
#
# This defines one target, WebExt::WebExt: the C ABI library (crates/webext-ffi) built with cargo, plus the include path
# for <webext.h>. Only the C ABI crosses the boundary, so the C++ side and the Rust side may use different allocators.
#
# Options:
#   WEBEXT_CARGO_WORKING_DIRECTORY  Directory to run cargo from. rustup picks the toolchain from the nearest
#                                   rust-toolchain.toml, so set this to the browser's source root to build with the same
#                                   compiler as the browser's own Rust crates (default: the browser's top-level source
#                                   directory, falling back to this repository).
#   WEBEXT_CARGO_EXTRA_ARGS         Extra arguments for `cargo build`, e.g. "--locked" or "--offline".
cmake_minimum_required(VERSION 3.25)

if (TARGET WebExt)
    return()
endif()

find_program(WEBEXT_CARGO cargo REQUIRED)

if (NOT WEBEXT_CARGO_WORKING_DIRECTORY)
    if (CMAKE_SOURCE_DIR AND NOT CMAKE_SOURCE_DIR STREQUAL CMAKE_CURRENT_SOURCE_DIR)
        set(WEBEXT_CARGO_WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}")
    else()
        set(WEBEXT_CARGO_WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}")
    endif()
endif()

string(TOUPPER "${CMAKE_BUILD_TYPE}" _webext_build_type)
if (_webext_build_type STREQUAL "DEBUG" OR _webext_build_type STREQUAL "")
    set(_webext_profile_flag "")
    set(_webext_profile_dir "debug")
else()
    set(_webext_profile_flag "--release")
    set(_webext_profile_dir "release")
endif()

set(_webext_target_dir "${CMAKE_CURRENT_BINARY_DIR}/cargo")
set(_webext_lib "${_webext_target_dir}/${_webext_profile_dir}/${CMAKE_STATIC_LIBRARY_PREFIX}webext_ffi${CMAKE_STATIC_LIBRARY_SUFFIX}")
if (WIN32)
    # MSVC-style toolchains name the archive without a prefix.
    set(_webext_lib "${_webext_target_dir}/${_webext_profile_dir}/webext_ffi.lib")
endif()

file(GLOB_RECURSE _webext_rust_sources CONFIGURE_DEPENDS
    "${CMAKE_CURRENT_SOURCE_DIR}/crates/*.rs"
    "${CMAKE_CURRENT_SOURCE_DIR}/crates/*/Cargo.toml"
    "${CMAKE_CURRENT_SOURCE_DIR}/Data/*.json"
    "${CMAKE_CURRENT_SOURCE_DIR}/Shim/*.js")

# Rust's release profile normally aborts on panic. The FFI layer contains panics with catch_unwind so a bug in an
# extension request cannot take the browser down, which needs unwinding.
add_custom_command(
    OUTPUT "${_webext_lib}"
    COMMAND ${CMAKE_COMMAND} -E env
        CARGO_TERM_PROGRESS_WHEN=never
        CARGO_PROFILE_RELEASE_PANIC=unwind
        CARGO_PROFILE_DEV_PANIC=unwind
        "${WEBEXT_CARGO}" build
            --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
            --package webext-ffi
            --lib
            --target-dir "${_webext_target_dir}"
            ${_webext_profile_flag}
            ${WEBEXT_CARGO_EXTRA_ARGS}
    WORKING_DIRECTORY "${WEBEXT_CARGO_WORKING_DIRECTORY}"
    DEPENDS ${_webext_rust_sources}
        "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
        "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.lock"
    COMMENT "Building webext (cargo, ${_webext_profile_dir})"
    COMMAND_EXPAND_LISTS
    VERBATIM
)
add_custom_target(webext_ffi-build DEPENDS "${_webext_lib}")

add_library(webext_ffi STATIC IMPORTED GLOBAL)
set_target_properties(webext_ffi PROPERTIES IMPORTED_LOCATION "${_webext_lib}")
add_dependencies(webext_ffi webext_ffi-build)

# What a Rust static library needs from the platform.
if (WIN32)
    set_property(TARGET webext_ffi APPEND PROPERTY INTERFACE_LINK_LIBRARIES kernel32 ntdll ws2_32 userenv bcrypt)
elseif (NOT APPLE)
    find_package(Threads REQUIRED)
    set_property(TARGET webext_ffi APPEND PROPERTY INTERFACE_LINK_LIBRARIES Threads::Threads ${CMAKE_DL_LIBS} m)
endif()

add_library(WebExt INTERFACE)
target_include_directories(WebExt INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}/crates/webext-ffi/include")
target_link_libraries(WebExt INTERFACE webext_ffi)
add_library(WebExt::WebExt ALIAS WebExt)

# The JavaScript shim and registry are plain files the browser embeds or ships; expose where they are.
set(WEBEXT_SHIM_JS "${CMAKE_CURRENT_SOURCE_DIR}/Shim/webext-shim.js" CACHE INTERNAL "Path to webext-shim.js")
set(WEBEXT_API_REGISTRY_JSON "${CMAKE_CURRENT_SOURCE_DIR}/Data/api-support.json" CACHE INTERNAL "Path to api-support.json")
