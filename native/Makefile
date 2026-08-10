SHELL := /bin/sh

ARCH ?= riscv64
HOST_CC ?= cc
CLANG ?= clang
RUSTC ?= rustc

REPO_ROOT := $(abspath ..)
NATIVE_MAKEFILE := $(abspath $(lastword $(MAKEFILE_LIST)))
OUTPUT ?= $(REPO_ROOT)/build/$(ARCH)/native
OBJECT_DIR := $(OUTPUT)/obj
INCLUDE_DIR := $(OUTPUT)/include
BINDING := $(INCLUDE_DIR)/mygo_program.h
MANIFEST := $(REPO_ROOT)/native/examples/hello/program.json
CHILD_C_DIR := $(REPO_ROOT)/native/examples/child
CHILD_C_MANIFEST := $(CHILD_C_DIR)/program.json
PARENT_C_DIR := $(REPO_ROOT)/native/examples/parent
PARENT_C_MANIFEST := $(PARENT_C_DIR)/program.json
OPTIONAL_MANIFEST := $(REPO_ROOT)/native/mrt/tests/optional-program.json
OPTIONAL_INCLUDE_DIR := $(INCLUDE_DIR)/optional
OPTIONAL_BINDING := $(OPTIONAL_INCLUDE_DIR)/mygo_program.h
PROCESS_TEST_MANIFEST := $(REPO_ROOT)/native/mrt/tests/process-program.json
PROCESS_TEST_INCLUDE_DIR := $(INCLUDE_DIR)/process-test
PROCESS_TEST_C_BINDING := $(PROCESS_TEST_INCLUDE_DIR)/mygo_program.h
PROCESS_TEST_RUST_BINDING := $(PROCESS_TEST_INCLUDE_DIR)/mygo_program.rs
COMPONENT_TEST_MANIFEST := $(REPO_ROOT)/native/mrt/tests/component-program.json
COMPONENT_TEST_INCLUDE_DIR := $(INCLUDE_DIR)/component-test
COMPONENT_TEST_BINDING := $(COMPONENT_TEST_INCLUDE_DIR)/mygo_program.h
RUST_HELLO_DIR := $(REPO_ROOT)/native/examples/rust-hello
RUST_MANIFEST := $(RUST_HELLO_DIR)/program.json
RUST_CHILD_DIR := $(REPO_ROOT)/native/examples/rust-child
RUST_CHILD_MANIFEST := $(RUST_CHILD_DIR)/program.json
RUST_CHILD_INCLUDE_DIR := $(INCLUDE_DIR)/rust-child
RUST_CHILD_C_BINDING := $(RUST_CHILD_INCLUDE_DIR)/mygo_program.h
RUST_CHILD_BINDING := $(RUST_CHILD_INCLUDE_DIR)/mygo_program.rs
RUST_CHILD_OBJECT_DIR := $(OBJECT_DIR)/rust-child
RUST_CHILD_RLIB := $(RUST_CHILD_OBJECT_DIR)/libanonlib.rlib
RUST_CHILD_ANONLIB := $(RUST_CHILD_OBJECT_DIR)/anonlib.o
RUST_PARENT_DIR := $(REPO_ROOT)/native/examples/rust-parent
RUST_PARENT_MANIFEST := $(RUST_PARENT_DIR)/program.json
RUST_INCLUDE_DIR := $(INCLUDE_DIR)/rust-hello
RUST_C_BINDING := $(RUST_INCLUDE_DIR)/mygo_program.h
RUST_BINDING := $(RUST_INCLUDE_DIR)/mygo_program.rs
RUST_OBJECT_DIR := $(OBJECT_DIR)/rust-hello
CHILD_C_INCLUDE_DIR := $(INCLUDE_DIR)/child
CHILD_C_BINDING := $(CHILD_C_INCLUDE_DIR)/mygo_program.h
CHILD_C_OBJECT_DIR := $(OBJECT_DIR)/child
PARENT_C_INCLUDE_DIR := $(INCLUDE_DIR)/parent
PARENT_C_BINDING := $(PARENT_C_INCLUDE_DIR)/mygo_program.h
PARENT_C_OBJECT_DIR := $(OBJECT_DIR)/parent
RUST_PARENT_INCLUDE_DIR := $(INCLUDE_DIR)/rust-parent
RUST_PARENT_C_BINDING := $(RUST_PARENT_INCLUDE_DIR)/mygo_program.h
RUST_PARENT_BINDING := $(RUST_PARENT_INCLUDE_DIR)/mygo_program.rs
RUST_PARENT_OBJECT_DIR := $(OBJECT_DIR)/rust-parent
COMPONENT_PLUGIN_DIR := $(REPO_ROOT)/native/examples/component-plugin
COMPONENT_PLUGIN_MANIFEST := $(COMPONENT_PLUGIN_DIR)/component.json
COMPONENT_PLUGIN_OBJECT := $(OBJECT_DIR)/component-plugin/plugin.o
COMPONENT_PLUGIN_SOYO := $(OUTPUT)/component-plugin.soyo
COMPONENT_HOST_DIR := $(REPO_ROOT)/native/examples/component-host
COMPONENT_HOST_MANIFEST := $(COMPONENT_HOST_DIR)/program.json
COMPONENT_HOST_INCLUDE_DIR := $(INCLUDE_DIR)/component-host
COMPONENT_HOST_BINDING := $(COMPONENT_HOST_INCLUDE_DIR)/mygo_program.h
COMPONENT_HOST_OBJECT_DIR := $(OBJECT_DIR)/component-host
COMPONENT_HOST_PLUGIN_HEADER := $(COMPONENT_HOST_OBJECT_DIR)/plugin_image.h
COMPONENT_HOST_PLUGIN_SOURCE := $(COMPONENT_HOST_OBJECT_DIR)/plugin-image.S
COMPONENT_HOST_PLUGIN_OBJECT := $(COMPONENT_HOST_OBJECT_DIR)/plugin-image.o
COMPONENT_HOST_OBJECT := $(COMPONENT_HOST_OBJECT_DIR)/main.o
COMPONENT_HOST_SOYO := $(OUTPUT)/component-host.soyo
RUST_COMPONENT_HOST_DIR := $(REPO_ROOT)/native/examples/rust-component-host
RUST_COMPONENT_HOST_BINDING := $(COMPONENT_HOST_INCLUDE_DIR)/mygo_program.rs
RUST_COMPONENT_HOST_OBJECT_DIR := $(COMPONENT_HOST_OBJECT_DIR)/rust
RUST_COMPONENT_HOST_RLIB := $(RUST_COMPONENT_HOST_OBJECT_DIR)/libanonlib.rlib
RUST_COMPONENT_HOST_ANONLIB := $(RUST_COMPONENT_HOST_OBJECT_DIR)/anonlib.o
RUST_COMPONENT_HOST_OBJECT := $(RUST_COMPONENT_HOST_OBJECT_DIR)/main.o
RUST_COMPONENT_HOST_SOYO := $(OUTPUT)/rust-component-host.soyo
RING_IO_DIR := $(REPO_ROOT)/native/examples/ring-io
RING_IO_MANIFEST := $(RING_IO_DIR)/program.json
RING_IO_INCLUDE_DIR := $(INCLUDE_DIR)/ring-io
RING_IO_BINDING := $(RING_IO_INCLUDE_DIR)/mygo_program.rs
RING_IO_C_BINDING := $(RING_IO_INCLUDE_DIR)/mygo_program.h
RING_IO_OBJECT_DIR := $(OBJECT_DIR)/ring-io
RING_IO_RLIB := $(RING_IO_OBJECT_DIR)/libanonlib.rlib
RING_IO_ANONLIB := $(RING_IO_OBJECT_DIR)/anonlib.o
RING_IO_OBJECT := $(RING_IO_OBJECT_DIR)/main.o
RING_IO_SOYO := $(OUTPUT)/ring-io.soyo
SOCKET_RING_DIR := $(REPO_ROOT)/native/examples/socket-ring
SOCKET_RING_MANIFEST := $(SOCKET_RING_DIR)/program.json
SOCKET_RING_INCLUDE_DIR := $(INCLUDE_DIR)/socket-ring
SOCKET_RING_BINDING := $(SOCKET_RING_INCLUDE_DIR)/mygo_program.rs
SOCKET_RING_C_BINDING := $(SOCKET_RING_INCLUDE_DIR)/mygo_program.h
SOCKET_RING_OBJECT_DIR := $(OBJECT_DIR)/socket-ring
SOCKET_RING_RLIB := $(SOCKET_RING_OBJECT_DIR)/libanonlib.rlib
SOCKET_RING_ANONLIB := $(SOCKET_RING_OBJECT_DIR)/anonlib.o
SOCKET_RING_OBJECT := $(SOCKET_RING_OBJECT_DIR)/main.o
SOCKET_RING_SOYO := $(OUTPUT)/socket-ring.soyo
DEVICE_RING_DIR := $(REPO_ROOT)/native/examples/device-ring
DEVICE_RING_MANIFEST := $(DEVICE_RING_DIR)/program.json
DEVICE_RING_INCLUDE_DIR := $(INCLUDE_DIR)/device-ring
DEVICE_RING_BINDING := $(DEVICE_RING_INCLUDE_DIR)/mygo_program.rs
DEVICE_RING_C_BINDING := $(DEVICE_RING_INCLUDE_DIR)/mygo_program.h
DEVICE_RING_OBJECT_DIR := $(OBJECT_DIR)/device-ring
DEVICE_RING_RLIB := $(DEVICE_RING_OBJECT_DIR)/libanonlib.rlib
DEVICE_RING_ANONLIB := $(DEVICE_RING_OBJECT_DIR)/anonlib.o
DEVICE_RING_OBJECT := $(DEVICE_RING_OBJECT_DIR)/main.o
DEVICE_RING_SOYO := $(OUTPUT)/device-ring.soyo
COMPONENT_REPOSITORY_DIR := $(REPO_ROOT)/native/services/component-repository
COMPONENT_REPOSITORY_MANIFEST := $(COMPONENT_REPOSITORY_DIR)/program.json
COMPONENT_REPOSITORY_INCLUDE_DIR := $(INCLUDE_DIR)/component-repository
COMPONENT_REPOSITORY_BINDING := $(COMPONENT_REPOSITORY_INCLUDE_DIR)/mygo_program.h
COMPONENT_REPOSITORY_OBJECT_DIR := $(OBJECT_DIR)/component-repository
COMPONENT_REPOSITORY_SOYO := $(OUTPUT)/component-repository.soyo
REPOSITORY_CLIENT_DIR := $(REPO_ROOT)/native/examples/repository-client
REPOSITORY_CLIENT_MANIFEST := $(REPOSITORY_CLIENT_DIR)/program.json
REPOSITORY_CLIENT_INCLUDE_DIR := $(INCLUDE_DIR)/repository-client
REPOSITORY_CLIENT_BINDING := $(REPOSITORY_CLIENT_INCLUDE_DIR)/mygo_program.h
REPOSITORY_CLIENT_OBJECT_DIR := $(OBJECT_DIR)/repository-client
REPOSITORY_CLIENT_IMAGES_HEADER := $(REPOSITORY_CLIENT_OBJECT_DIR)/images.h
REPOSITORY_CLIENT_IMAGES_SOURCE := $(REPOSITORY_CLIENT_OBJECT_DIR)/images.S
REPOSITORY_CLIENT_IMAGES_OBJECT := $(REPOSITORY_CLIENT_OBJECT_DIR)/images.o
REPOSITORY_CLIENT_SOYO := $(OUTPUT)/repository-client.soyo
ANONLIB_SOURCE := $(REPO_ROOT)/native/anonlib/src/lib.rs
ANONLIB_SOURCES := $(wildcard $(REPO_ROOT)/native/anonlib/src/*.rs)
ANONLIB_MANIFEST := $(REPO_ROOT)/native/anonlib/Cargo.toml
MRT_HEADER := $(REPO_ROOT)/native/include/mrt/mrt.h
RANALIB_ASSERT_HEADER := $(REPO_ROOT)/native/include/ranalib/assert.h
RANALIB_CTYPE_HEADER := $(REPO_ROOT)/native/include/ranalib/ctype.h
RANALIB_ERRNO_HEADER := $(REPO_ROOT)/native/include/ranalib/errno.h
RANALIB_INTTYPES_HEADER := $(REPO_ROOT)/native/include/ranalib/inttypes.h
RANALIB_LOCALE_HEADER := $(REPO_ROOT)/native/include/ranalib/locale.h
RANALIB_STDLIB_HEADER := $(REPO_ROOT)/native/include/ranalib/stdlib.h
RANALIB_STRING_HEADER := $(REPO_ROOT)/native/include/ranalib/string.h
RANALIB_STDIO_HEADER := $(REPO_ROOT)/native/include/ranalib/stdio.h
RANALIB_TIME_HEADER := $(REPO_ROOT)/native/include/ranalib/time.h
RANALIB_THREADS_HEADER := $(REPO_ROOT)/native/include/ranalib/threads.h
TLSF_DIR := $(REPO_ROOT)/native/ranalib/vendor/tlsf
TLSF_SOURCE := $(TLSF_DIR)/tlsf.c
TLSF_HEADER := $(TLSF_DIR)/tlsf.h
TLSF_CONFIG := $(REPO_ROOT)/native/ranalib/src/tlsf_config.h

HOST_TARGET := x86_64-unknown-linux-gnu
SOYO_LINKER_TARGET := $(REPO_ROOT)/tools/soyo-linker/target
SOYO_LD ?= $(SOYO_LINKER_TARGET)/$(HOST_TARGET)/release/soyo-ld
SOYO_LINKER_INPUTS := $(shell find \
	$(REPO_ROOT)/Cargo.toml \
	$(REPO_ROOT)/tools/soyo-linker/src \
	$(REPO_ROOT)/tools/soyo-linker/Cargo.toml \
	$(REPO_ROOT)/libs/native-abi/src \
	$(REPO_ROOT)/libs/native-abi/Cargo.toml \
	$(REPO_ROOT)/libs/soyo/src \
	$(REPO_ROOT)/libs/soyo/Cargo.toml \
	$(REPO_ROOT)/libs/soyo/build.rs \
	$(REPO_ROOT)/libs/soyo/wire-abi.registry \
	-type f -print)

COMMON_CFLAGS := -std=c11 -ffreestanding -fno-builtin -fno-pic -fno-pie \
	-fno-stack-protector -fno-asynchronous-unwind-tables -fno-unwind-tables \
	-fvisibility=hidden -Wall -Wextra -Werror -O2 \
	-I$(INCLUDE_DIR) -I$(REPO_ROOT)/native/include
COMMON_ASFLAGS := -ffreestanding -fno-pic -fno-pie -fno-asynchronous-unwind-tables \
	-fno-unwind-tables -fvisibility=hidden
COMMON_RUSTFLAGS := --edition=2024 -C opt-level=2 -C panic=abort \
	-C relocation-model=static -C code-model=medium -C codegen-units=1 -C debuginfo=0
ANONLIB_METADATA := mygo_native_anonlib
ANONLIB_PARENT_METADATA := mygo_native_anonlib_parent
ANONLIB_CHILD_METADATA := mygo_native_anonlib_child
RUST_HELLO_METADATA := mygo_native_rust_hello
RUST_CHILD_METADATA := mygo_native_rust_child
RUST_COMPONENT_HOST_METADATA := mygo_native_rust_component_host
ANONLIB_COMPONENT_METADATA := mygo_native_anonlib_component
RUST_PARENT_METADATA := mygo_native_rust_parent
ANONLIB_RING_IO_METADATA := mygo_native_anonlib_ring_io
RING_IO_METADATA := mygo_native_ring_io
ANONLIB_SOCKET_RING_METADATA := mygo_native_anonlib_socket_ring
SOCKET_RING_METADATA := mygo_native_socket_ring
ANONLIB_DEVICE_RING_METADATA := mygo_native_anonlib_device_ring
DEVICE_RING_METADATA := mygo_native_device_ring

ifeq ($(ARCH),riscv64)
SOYO_TARGET := riscv64
TARGET_TRIPLE := riscv64-unknown-none-elf
TARGET_FLAGS := -mno-relax -msmall-data-limit=0 -mcmodel=medany
RUST_TARGET_TRIPLE := riscv64imac-unknown-none-elf
ENTRY_SOURCE := $(REPO_ROOT)/native/mrt/arch/riscv64/entry.S
else ifeq ($(ARCH),loongarch64)
SOYO_TARGET := loongarch64
TARGET_TRIPLE := loongarch64-unknown-none
TARGET_FLAGS := -fdirect-access-external-data
RUST_TARGET_TRIPLE := loongarch64-unknown-none
ENTRY_SOURCE := $(REPO_ROOT)/native/mrt/arch/loongarch64/entry.S
else
$(error ARCH 必须为 riscv64 或 loongarch64)
endif

MRT_OBJECTS := \
	$(OBJECT_DIR)/mrt-entry.o \
	$(OBJECT_DIR)/mrt-start.o \
	$(OBJECT_DIR)/mrt-start-info.o \
	$(OBJECT_DIR)/mrt-lifecycle.o \
	$(OBJECT_DIR)/mrt-process.o \
	$(OBJECT_DIR)/mrt-component.o \
	$(OBJECT_DIR)/mrt-entry-c.o
RANALIB_OBJECTS := \
	$(OBJECT_DIR)/ranalib-assert.o \
	$(OBJECT_DIR)/ranalib-ctype.o \
	$(OBJECT_DIR)/ranalib-errno.o \
	$(OBJECT_DIR)/ranalib-exit.o \
	$(OBJECT_DIR)/ranalib-tlsf.o \
	$(OBJECT_DIR)/ranalib-heap.o \
	$(OBJECT_DIR)/ranalib-inttypes.o \
	$(OBJECT_DIR)/ranalib-locale.o \
	$(OBJECT_DIR)/ranalib-stdlib.o \
	$(OBJECT_DIR)/ranalib-string.o \
	$(OBJECT_DIR)/ranalib-stdio.o \
	$(OBJECT_DIR)/ranalib-time.o \
	$(OBJECT_DIR)/ranalib-threads.o
HELLO_OBJECT := $(OBJECT_DIR)/hello.o
HELLO_SOYO := $(OUTPUT)/hello.soyo
RUST_MRT_OBJECTS := \
	$(OBJECT_DIR)/mrt-entry.o \
	$(RUST_OBJECT_DIR)/mrt-start.o \
	$(RUST_OBJECT_DIR)/mrt-start-info.o \
	$(RUST_OBJECT_DIR)/mrt-lifecycle.o \
	$(RUST_OBJECT_DIR)/mrt-process.o \
	$(RUST_OBJECT_DIR)/mrt-component.o \
	$(RUST_OBJECT_DIR)/mrt-entry-rust.o
ANONLIB_RLIB := $(RUST_OBJECT_DIR)/libanonlib.rlib
ANONLIB_OBJECT := $(RUST_OBJECT_DIR)/anonlib.o
RUST_HELLO_OBJECT := $(RUST_OBJECT_DIR)/rust-hello.o
RUST_HELLO_SOYO := $(OUTPUT)/rust-hello.soyo
C_BUILD_CONFIG := $(OBJECT_DIR)/c-build-config.stamp
CHILD_C_SOYO := $(OUTPUT)/child.soyo
PARENT_C_SOYO := $(OUTPUT)/parent.soyo
RUST_CHILD_SOYO := $(OUTPUT)/rust-child.soyo
RUST_PARENT_SOYO := $(OUTPUT)/rust-parent.soyo
CHILD_C_OBJECT := $(CHILD_C_OBJECT_DIR)/child.o
PARENT_C_OBJECT := $(PARENT_C_OBJECT_DIR)/parent.o
RUST_CHILD_OBJECT := $(RUST_CHILD_OBJECT_DIR)/rust-child.o
RUST_PARENT_OBJECT := $(RUST_PARENT_OBJECT_DIR)/rust-parent.o
PARENT_C_CHILD_IMAGE_SOURCE := $(PARENT_C_OBJECT_DIR)/child-image.S
PARENT_C_CHILD_IMAGE_OBJECT := $(PARENT_C_OBJECT_DIR)/child-image.o
PARENT_C_CHILD_IMAGE_HEADER := $(PARENT_C_OBJECT_DIR)/child_image.h
RUST_PARENT_CHILD_IMAGE_SOURCE := $(RUST_PARENT_OBJECT_DIR)/child-image.S
RUST_PARENT_CHILD_IMAGE_OBJECT := $(RUST_PARENT_OBJECT_DIR)/child-image.o
RUST_BUILD_CONFIG := $(RUST_OBJECT_DIR)/rust-build-config.stamp
RUST_PARENT_BUILD_CONFIG := $(RUST_PARENT_OBJECT_DIR)/rust-build-config.stamp
HOST_START_INFO_TEST := $(OUTPUT)/mrt-start-info-test
HOST_OPTIONAL_START_INFO_TEST := $(OUTPUT)/mrt-optional-start-info-test
HOST_ABORT_TEST := $(OUTPUT)/mrt-abort-test
HOST_LIFECYCLE_TEST := $(OUTPUT)/mrt-lifecycle-test
HOST_ENTRY_C_TEST := $(OUTPUT)/mrt-entry-c-test
HOST_PROCESS_TEST := $(OUTPUT)/mrt-process-test
HOST_COMPONENT_TEST := $(OUTPUT)/mrt-component-test
HOST_ERRNO_TEST := $(OUTPUT)/ranalib-errno-test
HOST_HEAP_TEST := $(OUTPUT)/ranalib-heap-test
HOST_STRING_TEST := $(OUTPUT)/ranalib-string-test
HOST_STDIO_TEST := $(OUTPUT)/ranalib-stdio-test
HOST_STDLIB_TEST := $(OUTPUT)/ranalib-stdlib-test
HOST_CTYPE_LOCALE_TEST := $(OUTPUT)/ranalib-ctype-locale-test
HOST_THREADS_TEST := $(OUTPUT)/ranalib-threads-test
HOST_COMPONENT_REPOSITORY_PATH_TEST := $(OUTPUT)/component-repository-path-test

.PHONY: hello rust-hello native-parent native-rust-parent native-parents component ring-io socket-ring device-ring \
	rust-component \
	component-repository repository-client mrt test FORCE

FORCE:

hello: $(HELLO_SOYO)

rust-hello: $(RUST_HELLO_SOYO)

native-parent: $(PARENT_C_SOYO)

native-rust-parent: $(RUST_PARENT_SOYO)

native-parents: $(CHILD_C_SOYO) $(PARENT_C_SOYO) $(RUST_CHILD_SOYO) $(RUST_PARENT_SOYO)

component: $(COMPONENT_PLUGIN_SOYO) $(COMPONENT_HOST_SOYO) $(RUST_COMPONENT_HOST_SOYO)

rust-component: $(COMPONENT_PLUGIN_SOYO) $(RUST_COMPONENT_HOST_SOYO)

ring-io: $(RING_IO_SOYO)

socket-ring: $(SOCKET_RING_SOYO)

device-ring: $(DEVICE_RING_SOYO)

component-repository: $(COMPONENT_REPOSITORY_SOYO)

repository-client: $(REPOSITORY_CLIENT_SOYO)

mrt: $(MRT_OBJECTS)

test: $(HOST_START_INFO_TEST) $(HOST_OPTIONAL_START_INFO_TEST) $(HOST_ABORT_TEST) $(HOST_LIFECYCLE_TEST) $(HOST_ENTRY_C_TEST) $(HOST_PROCESS_TEST) $(HOST_COMPONENT_TEST) $(HOST_ERRNO_TEST) $(HOST_HEAP_TEST) $(HOST_STRING_TEST) $(HOST_STDIO_TEST) $(HOST_STDLIB_TEST) $(HOST_CTYPE_LOCALE_TEST) $(HOST_THREADS_TEST) $(HOST_COMPONENT_REPOSITORY_PATH_TEST) $(PROCESS_TEST_RUST_BINDING)
	$(HOST_START_INFO_TEST)
	$(HOST_OPTIONAL_START_INFO_TEST)
	$(HOST_ABORT_TEST)
	$(HOST_LIFECYCLE_TEST)
	$(HOST_ENTRY_C_TEST)
	$(HOST_PROCESS_TEST)
	$(HOST_COMPONENT_TEST)
	$(HOST_ERRNO_TEST)
	$(HOST_HEAP_TEST)
	$(HOST_STRING_TEST)
	$(HOST_STDIO_TEST)
	$(HOST_STDLIB_TEST)
	$(HOST_CTYPE_LOCALE_TEST)
	$(HOST_THREADS_TEST)
	$(HOST_COMPONENT_REPOSITORY_PATH_TEST)
	MYGO_PROGRAM_RS=$(abspath $(PROCESS_TEST_RUST_BINDING)) \
		CARGO_TARGET_DIR=$(OUTPUT)/anonlib-target \
		cargo test --manifest-path $(ANONLIB_MANIFEST) --target $(HOST_TARGET)

$(SOYO_LD): $(SOYO_LINKER_INPUTS)
	CARGO_TARGET_DIR=$(SOYO_LINKER_TARGET) \
		cargo build --manifest-path $(REPO_ROOT)/tools/soyo-linker/Cargo.toml \
		--target $(HOST_TARGET) --release

$(C_BUILD_CONFIG): FORCE
	@mkdir -p $(OBJECT_DIR)
	@set -eu; \
		tmp="$@.tmp"; \
		{ \
			printf '%s\n' \
				'arch=$(ARCH)' \
				'soyo_target=$(SOYO_TARGET)' \
				'target_triple=$(TARGET_TRIPLE)' \
				'target_flags=$(TARGET_FLAGS)' \
				'clang=$(CLANG)' \
				'common_cflags=$(COMMON_CFLAGS)' \
				'common_asflags=$(COMMON_ASFLAGS)'; \
			printf '%s\n' 'makefile.cksum'; \
			set -- $$(cksum "$(NATIVE_MAKEFILE)"); \
			printf '%s %s\n' "$$1" "$$2"; \
			printf '%s\n' 'clang.version'; \
			$(CLANG) --version; \
		} > "$$tmp"; \
		if ! cmp -s "$$tmp" "$@"; then \
			mv "$$tmp" "$@"; \
		else \
			rm -f "$$tmp"; \
		fi

$(RUST_BUILD_CONFIG): FORCE
	@mkdir -p $(RUST_OBJECT_DIR)
	@set -eu; \
		tmp="$@.tmp"; \
		{ \
			printf '%s\n' \
				'arch=$(ARCH)' \
				'rust_target_triple=$(RUST_TARGET_TRIPLE)' \
				'rustc=$(RUSTC)' \
				'common_rustflags=$(COMMON_RUSTFLAGS)' \
				'anonlib_metadata=$(ANONLIB_METADATA)' \
				'rust_hello_metadata=$(RUST_HELLO_METADATA)' \
				'anonlib_component_metadata=$(ANONLIB_COMPONENT_METADATA)' \
				'rust_component_host_metadata=$(RUST_COMPONENT_HOST_METADATA)'; \
			printf '%s\n' 'makefile.cksum'; \
			set -- $$(cksum "$(NATIVE_MAKEFILE)"); \
			printf '%s %s\n' "$$1" "$$2"; \
			printf '%s\n' 'rustc.version'; \
			$(RUSTC) --version; \
		} > "$$tmp"; \
		if ! cmp -s "$$tmp" "$@"; then \
			mv "$$tmp" "$@"; \
		else \
			rm -f "$$tmp"; \
		fi

$(BINDING): $(SOYO_LD) $(MANIFEST)
	mkdir -p $(INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(MANIFEST) \
		--emit-c-header $@

$(OPTIONAL_BINDING): $(SOYO_LD) $(OPTIONAL_MANIFEST)
	mkdir -p $(OPTIONAL_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(OPTIONAL_MANIFEST) \
		--emit-c-header $@

$(PROCESS_TEST_C_BINDING): $(SOYO_LD) $(PROCESS_TEST_MANIFEST)
	mkdir -p $(PROCESS_TEST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(PROCESS_TEST_MANIFEST) \
		--emit-c-header $@

$(PROCESS_TEST_RUST_BINDING): $(SOYO_LD) $(PROCESS_TEST_MANIFEST)
	mkdir -p $(PROCESS_TEST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(PROCESS_TEST_MANIFEST) \
		--emit-rust-module $@

$(COMPONENT_TEST_BINDING): $(SOYO_LD) $(COMPONENT_TEST_MANIFEST)
	mkdir -p $(COMPONENT_TEST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_TEST_MANIFEST) \
		--emit-c-header $@

$(RUST_C_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
		--emit-c-header $@

$(RUST_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
		--emit-rust-module $@

$(RUST_CHILD_C_BINDING): $(SOYO_LD) $(RUST_CHILD_MANIFEST)
	mkdir -p $(RUST_CHILD_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_CHILD_MANIFEST) \
		--emit-c-header $@

$(RUST_CHILD_BINDING): $(SOYO_LD) $(RUST_CHILD_MANIFEST)
	mkdir -p $(RUST_CHILD_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_CHILD_MANIFEST) \
		--emit-rust-module $@

$(CHILD_C_BINDING): $(SOYO_LD) $(CHILD_C_MANIFEST)
	mkdir -p $(CHILD_C_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(CHILD_C_MANIFEST) \
		--emit-c-header $@

$(PARENT_C_BINDING): $(SOYO_LD) $(PARENT_C_MANIFEST)
	mkdir -p $(PARENT_C_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(PARENT_C_MANIFEST) \
		--emit-c-header $@

$(RUST_PARENT_C_BINDING): $(SOYO_LD) $(RUST_PARENT_MANIFEST)
	mkdir -p $(RUST_PARENT_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_PARENT_MANIFEST) \
		--emit-c-header $@

$(RUST_PARENT_BINDING): $(SOYO_LD) $(RUST_PARENT_MANIFEST)
	mkdir -p $(RUST_PARENT_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_PARENT_MANIFEST) \
		--emit-rust-module $@

$(COMPONENT_HOST_BINDING): $(SOYO_LD) $(COMPONENT_HOST_MANIFEST)
	mkdir -p $(COMPONENT_HOST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_HOST_MANIFEST) \
		--emit-c-header $@

$(RUST_COMPONENT_HOST_BINDING): $(SOYO_LD) $(COMPONENT_HOST_MANIFEST)
	mkdir -p $(COMPONENT_HOST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_HOST_MANIFEST) \
		--emit-rust-module $@

$(RING_IO_BINDING): $(SOYO_LD) $(RING_IO_MANIFEST)
	mkdir -p $(RING_IO_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RING_IO_MANIFEST) \
		--emit-rust-module $@

$(RING_IO_C_BINDING): $(SOYO_LD) $(RING_IO_MANIFEST)
	mkdir -p $(RING_IO_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RING_IO_MANIFEST) \
		--emit-c-header $@

$(SOCKET_RING_BINDING): $(SOYO_LD) $(SOCKET_RING_MANIFEST)
	mkdir -p $(SOCKET_RING_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(SOCKET_RING_MANIFEST) \
		--emit-rust-module $@

$(SOCKET_RING_C_BINDING): $(SOYO_LD) $(SOCKET_RING_MANIFEST)
	mkdir -p $(SOCKET_RING_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(SOCKET_RING_MANIFEST) \
		--emit-c-header $@

$(DEVICE_RING_BINDING): $(SOYO_LD) $(DEVICE_RING_MANIFEST)
	mkdir -p $(DEVICE_RING_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(DEVICE_RING_MANIFEST) \
		--emit-rust-module $@

$(DEVICE_RING_C_BINDING): $(SOYO_LD) $(DEVICE_RING_MANIFEST)
	mkdir -p $(DEVICE_RING_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(DEVICE_RING_MANIFEST) \
		--emit-c-header $@

$(COMPONENT_REPOSITORY_BINDING): $(SOYO_LD) $(COMPONENT_REPOSITORY_MANIFEST)
	mkdir -p $(COMPONENT_REPOSITORY_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_REPOSITORY_MANIFEST) \
		--emit-c-header $@

$(REPOSITORY_CLIENT_BINDING): $(SOYO_LD) $(REPOSITORY_CLIENT_MANIFEST)
	mkdir -p $(REPOSITORY_CLIENT_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(REPOSITORY_CLIENT_MANIFEST) \
		--emit-c-header $@

$(OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-component.o: $(REPO_ROOT)/native/mrt/src/component.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-entry-c.o: $(REPO_ROOT)/native/mrt/src/entry_c.c $(BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-component.o: $(REPO_ROOT)/native/mrt/src/component.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-entry-rust.o: $(REPO_ROOT)/native/mrt/src/entry_rust.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/mrt-entry-c.o: $(REPO_ROOT)/native/mrt/src/entry_c.c $(CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/mrt-entry-c.o: $(REPO_ROOT)/native/mrt/src/entry_c.c $(PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(CHILD_C_BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-ctype.o: $(REPO_ROOT)/native/ranalib/src/ctype.c $(RANALIB_CTYPE_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-stdlib.o: $(REPO_ROOT)/native/ranalib/src/stdlib.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(PARENT_C_BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-ctype.o: $(REPO_ROOT)/native/ranalib/src/ctype.c $(RANALIB_CTYPE_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(PARENT_C_BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(PARENT_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-stdlib.o: $(REPO_ROOT)/native/ranalib/src/stdlib.c $(PARENT_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-component.o: $(REPO_ROOT)/native/mrt/src/component.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o: $(REPO_ROOT)/native/mrt/src/entry_rust.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(ANONLIB_RLIB): $(ANONLIB_SOURCES) $(RUST_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_METADATA) -o $@

$(ANONLIB_OBJECT): $(ANONLIB_SOURCES) $(RUST_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_METADATA) -o $@

$(RUST_HELLO_OBJECT): $(RUST_HELLO_DIR)/main.rs $(ANONLIB_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(RUSTC) $(RUST_HELLO_DIR)/main.rs --crate-name soyo_rust_hello \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RUST_HELLO_METADATA) --extern anonlib=$(ANONLIB_RLIB) -o $@

$(RUST_CHILD_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_CHILD_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_CHILD_OBJECT_DIR)/mrt-%.o: $(REPO_ROOT)/native/mrt/src/%.c $(RUST_CHILD_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_CHILD_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_CHILD_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_CHILD_RLIB): $(ANONLIB_SOURCES) $(RUST_CHILD_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_CHILD_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_CHILD_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_CHILD_METADATA) -o $@

$(RUST_CHILD_ANONLIB): $(ANONLIB_SOURCES) $(RUST_CHILD_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_CHILD_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_CHILD_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_CHILD_METADATA) -o $@

$(RUST_CHILD_OBJECT): $(RUST_CHILD_DIR)/main.rs $(RUST_CHILD_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_CHILD_OBJECT_DIR)
	$(RUSTC) $(RUST_CHILD_DIR)/main.rs --crate-name soyo_rust_child \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RUST_CHILD_METADATA) --extern anonlib=$(RUST_CHILD_RLIB) -o $@

$(RUST_PARENT_OBJECT_DIR)/libanonlib.rlib: $(ANONLIB_SOURCES) $(RUST_PARENT_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_PARENT_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_PARENT_METADATA) -o $@

$(RUST_PARENT_OBJECT_DIR)/anonlib.o: $(ANONLIB_SOURCES) $(RUST_PARENT_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_PARENT_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_PARENT_METADATA) -o $@

$(RUST_PARENT_OBJECT): $(RUST_PARENT_DIR)/main.rs $(RUST_PARENT_OBJECT_DIR)/libanonlib.rlib $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(RUSTC) $(RUST_PARENT_DIR)/main.rs --crate-name soyo_rust_parent \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RUST_PARENT_METADATA) --extern anonlib=$(RUST_PARENT_OBJECT_DIR)/libanonlib.rlib -o $@

$(OBJECT_DIR)/ranalib-assert.o: $(REPO_ROOT)/native/ranalib/src/assert.c $(BINDING) $(MRT_HEADER) $(RANALIB_ASSERT_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-ctype.o: $(REPO_ROOT)/native/ranalib/src/ctype.c $(RANALIB_CTYPE_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-tlsf.o: $(TLSF_SOURCE) $(TLSF_HEADER) $(TLSF_CONFIG) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) \
		-I$(TLSF_DIR)/freestanding -include $(TLSF_CONFIG) -c $< -o $@

$(OBJECT_DIR)/ranalib-heap.o: $(REPO_ROOT)/native/ranalib/src/heap.c $(TLSF_HEADER) $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-inttypes.o: $(REPO_ROOT)/native/ranalib/src/inttypes.c $(RANALIB_INTTYPES_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-locale.o: $(REPO_ROOT)/native/ranalib/src/locale.c $(RANALIB_LOCALE_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-stdlib.o: $(REPO_ROOT)/native/ranalib/src/stdlib.c $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(BINDING) $(MRT_HEADER) $(RANALIB_CTYPE_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-time.o: $(REPO_ROOT)/native/ranalib/src/time.c $(BINDING) $(MRT_HEADER) $(RANALIB_TIME_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-threads.o: $(REPO_ROOT)/native/ranalib/src/threads.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_THREADS_HEADER) $(RANALIB_TIME_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_OBJECT): $(REPO_ROOT)/native/examples/hello/main.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_SOYO): $(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT) $(MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(MANIFEST) -o $@ \
		$(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT)

$(RUST_HELLO_SOYO): $(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_HELLO_OBJECT) \
		$(RUST_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) -o $@ \
		$(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_HELLO_OBJECT)

$(CHILD_C_OBJECT): $(CHILD_C_DIR)/main.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) -I$(CHILD_C_INCLUDE_DIR) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_SOYO): \
		$(CHILD_C_OBJECT_DIR)/mrt-entry.o $(CHILD_C_OBJECT_DIR)/mrt-start.o \
		$(CHILD_C_OBJECT_DIR)/mrt-start-info.o $(CHILD_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(CHILD_C_OBJECT_DIR)/mrt-process.o $(CHILD_C_OBJECT_DIR)/mrt-entry-c.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-ctype.o $(CHILD_C_OBJECT_DIR)/ranalib-errno.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-exit.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-stdlib.o $(CHILD_C_OBJECT_DIR)/ranalib-string.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-stdio.o $(CHILD_C_OBJECT) $(CHILD_C_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(CHILD_C_MANIFEST) -o $@ \
		$(CHILD_C_OBJECT_DIR)/mrt-entry.o $(CHILD_C_OBJECT_DIR)/mrt-start.o \
		$(CHILD_C_OBJECT_DIR)/mrt-start-info.o $(CHILD_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(CHILD_C_OBJECT_DIR)/mrt-process.o $(CHILD_C_OBJECT_DIR)/mrt-entry-c.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-ctype.o $(CHILD_C_OBJECT_DIR)/ranalib-errno.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-exit.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-stdlib.o $(CHILD_C_OBJECT_DIR)/ranalib-string.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-stdio.o $(CHILD_C_OBJECT)

$(PARENT_C_CHILD_IMAGE_HEADER): $(PARENT_C_DIR)/child_image.h.in
	mkdir -p $(PARENT_C_OBJECT_DIR)
	cp $< $@

$(PARENT_C_CHILD_IMAGE_SOURCE): $(CHILD_C_SOYO)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	printf '%s\n' '.section .rodata' '.balign 8' '.globl mygo_child_image_start' 'mygo_child_image_start:' > $@
	printf '%s\n' '.incbin "$(CHILD_C_SOYO)"' '.globl mygo_child_image_end' 'mygo_child_image_end:' >> $@

$(PARENT_C_CHILD_IMAGE_OBJECT): $(PARENT_C_CHILD_IMAGE_SOURCE)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT): $(PARENT_C_DIR)/main.c $(PARENT_C_BINDING) $(PARENT_C_CHILD_IMAGE_HEADER) $(MRT_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) -I$(PARENT_C_OBJECT_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_SOYO): \
		$(PARENT_C_OBJECT_DIR)/mrt-entry.o $(PARENT_C_OBJECT_DIR)/mrt-start.o \
		$(PARENT_C_OBJECT_DIR)/mrt-start-info.o $(PARENT_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(PARENT_C_OBJECT_DIR)/mrt-process.o $(PARENT_C_OBJECT_DIR)/mrt-entry-c.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-ctype.o $(PARENT_C_OBJECT_DIR)/ranalib-errno.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-exit.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdlib.o $(PARENT_C_OBJECT_DIR)/ranalib-string.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o $(PARENT_C_OBJECT) \
		$(PARENT_C_CHILD_IMAGE_OBJECT) $(PARENT_C_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(PARENT_C_MANIFEST) -o $@ \
		$(PARENT_C_OBJECT_DIR)/mrt-entry.o $(PARENT_C_OBJECT_DIR)/mrt-start.o \
		$(PARENT_C_OBJECT_DIR)/mrt-start-info.o $(PARENT_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(PARENT_C_OBJECT_DIR)/mrt-process.o $(PARENT_C_OBJECT_DIR)/mrt-entry-c.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-ctype.o $(PARENT_C_OBJECT_DIR)/ranalib-errno.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-exit.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdlib.o $(PARENT_C_OBJECT_DIR)/ranalib-string.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o $(PARENT_C_OBJECT) \
		$(PARENT_C_CHILD_IMAGE_OBJECT)

RUST_CHILD_OBJECTS := \
	$(RUST_CHILD_OBJECT_DIR)/mrt-entry.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-start.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-start_info.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-lifecycle.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-process.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-component.o \
	$(RUST_CHILD_OBJECT_DIR)/mrt-entry_rust.o \
	$(RUST_CHILD_ANONLIB) \
	$(RUST_CHILD_OBJECT)

$(RUST_CHILD_SOYO): $(RUST_CHILD_OBJECTS) \
		$(RUST_CHILD_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_CHILD_MANIFEST) -o $@ \
		$(RUST_CHILD_OBJECTS)

$(RUST_PARENT_CHILD_IMAGE_SOURCE): $(RUST_CHILD_SOYO)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	printf '%s\n' '.section .rodata' '.balign 8' '.globl mygo_child_image_start' 'mygo_child_image_start:' > $@
	printf '%s\n' '.incbin "$(RUST_CHILD_SOYO)"' '.globl mygo_child_image_end' 'mygo_child_image_end:' >> $@

$(RUST_PARENT_CHILD_IMAGE_OBJECT): $(RUST_PARENT_CHILD_IMAGE_SOURCE)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_SOYO): \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry.o $(RUST_PARENT_OBJECT_DIR)/mrt-start.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-start-info.o $(RUST_PARENT_OBJECT_DIR)/mrt-lifecycle.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-process.o $(RUST_PARENT_OBJECT_DIR)/mrt-component.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o \
		$(RUST_PARENT_OBJECT_DIR)/anonlib.o $(RUST_PARENT_OBJECT) \
		$(RUST_PARENT_CHILD_IMAGE_OBJECT) $(RUST_PARENT_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_PARENT_MANIFEST) -o $@ \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry.o $(RUST_PARENT_OBJECT_DIR)/mrt-start.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-start-info.o $(RUST_PARENT_OBJECT_DIR)/mrt-lifecycle.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-process.o $(RUST_PARENT_OBJECT_DIR)/mrt-component.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o \
		$(RUST_PARENT_OBJECT_DIR)/anonlib.o $(RUST_PARENT_OBJECT) \
		$(RUST_PARENT_CHILD_IMAGE_OBJECT)

$(COMPONENT_PLUGIN_OBJECT): $(COMPONENT_PLUGIN_DIR)/plugin.c $(C_BUILD_CONFIG)
	mkdir -p $(dir $@)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_PLUGIN_SOYO): $(COMPONENT_PLUGIN_OBJECT) $(COMPONENT_PLUGIN_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --component --target $(SOYO_TARGET) --manifest $(COMPONENT_PLUGIN_MANIFEST) \
		-o $@ $(COMPONENT_PLUGIN_OBJECT)

$(COMPONENT_HOST_PLUGIN_HEADER): $(COMPONENT_HOST_DIR)/plugin_image.h.in
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	cp $< $@

$(COMPONENT_HOST_PLUGIN_SOURCE): $(COMPONENT_PLUGIN_SOYO)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	printf '%s\n' '.section .rodata' '.balign 8' '.globl mygo_plugin_image_start' 'mygo_plugin_image_start:' > $@
	printf '%s\n' '.incbin "$(COMPONENT_PLUGIN_SOYO)"' '.globl mygo_plugin_image_end' 'mygo_plugin_image_end:' >> $@

$(COMPONENT_HOST_PLUGIN_OBJECT): $(COMPONENT_HOST_PLUGIN_SOURCE)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-component.o: $(REPO_ROOT)/native/mrt/src/component.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry-c.o: $(REPO_ROOT)/native/mrt/src/entry_c.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(COMPONENT_HOST_BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-ctype.o: $(REPO_ROOT)/native/ranalib/src/ctype.c $(RANALIB_CTYPE_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-stdlib.o: $(REPO_ROOT)/native/ranalib/src/stdlib.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(RANALIB_CTYPE_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_HOST_OBJECT): $(COMPONENT_HOST_DIR)/main.c $(COMPONENT_HOST_BINDING) $(COMPONENT_HOST_PLUGIN_HEADER) $(MRT_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) -I$(COMPONENT_HOST_OBJECT_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

COMPONENT_HOST_OBJECTS := \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-start.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-start-info.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-lifecycle.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-process.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-component.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry-c.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-errno.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-exit.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-string.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-ctype.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-stdlib.o \
	$(COMPONENT_HOST_OBJECT_DIR)/ranalib-stdio.o \
	$(COMPONENT_HOST_OBJECT) \
	$(COMPONENT_HOST_PLUGIN_OBJECT)

$(COMPONENT_HOST_SOYO): $(COMPONENT_HOST_OBJECTS) $(COMPONENT_HOST_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_HOST_MANIFEST) -o $@ \
		$(COMPONENT_HOST_OBJECTS)

$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry-rust.o: $(REPO_ROOT)/native/mrt/src/entry_rust.c $(COMPONENT_HOST_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_HOST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_HOST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_COMPONENT_HOST_RLIB): $(ANONLIB_SOURCES) $(RUST_COMPONENT_HOST_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_COMPONENT_HOST_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_COMPONENT_HOST_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_COMPONENT_METADATA) -o $@

$(RUST_COMPONENT_HOST_ANONLIB): $(ANONLIB_SOURCES) $(RUST_COMPONENT_HOST_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_COMPONENT_HOST_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_COMPONENT_HOST_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_COMPONENT_METADATA) -o $@

$(RUST_COMPONENT_HOST_OBJECT): $(RUST_COMPONENT_HOST_DIR)/main.rs $(RUST_COMPONENT_HOST_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_COMPONENT_HOST_OBJECT_DIR)
	$(RUSTC) $(RUST_COMPONENT_HOST_DIR)/main.rs --crate-name soyo_rust_component_host \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RUST_COMPONENT_HOST_METADATA) \
		--extern anonlib=$(RUST_COMPONENT_HOST_RLIB) -o $@

RUST_COMPONENT_HOST_OBJECTS := \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-start.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-start-info.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-lifecycle.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-process.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-component.o \
	$(COMPONENT_HOST_OBJECT_DIR)/mrt-entry-rust.o \
	$(RUST_COMPONENT_HOST_ANONLIB) \
	$(RUST_COMPONENT_HOST_OBJECT) \
	$(COMPONENT_HOST_PLUGIN_OBJECT)

$(RUST_COMPONENT_HOST_SOYO): $(RUST_COMPONENT_HOST_OBJECTS) $(COMPONENT_HOST_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_HOST_MANIFEST) -o $@ \
		$(RUST_COMPONENT_HOST_OBJECTS)

$(RING_IO_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(RING_IO_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RING_IO_OBJECT_DIR)/mrt-%.o: $(REPO_ROOT)/native/mrt/src/%.c $(RING_IO_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RING_IO_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RING_IO_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RING_IO_RLIB): $(ANONLIB_SOURCES) $(RING_IO_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RING_IO_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RING_IO_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_RING_IO_METADATA) -o $@

$(RING_IO_ANONLIB): $(ANONLIB_SOURCES) $(RING_IO_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RING_IO_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RING_IO_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_RING_IO_METADATA) -o $@

$(RING_IO_OBJECT): $(RING_IO_DIR)/main.rs $(RING_IO_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(RING_IO_OBJECT_DIR)
	$(RUSTC) $(RING_IO_DIR)/main.rs --crate-name soyo_ring_io \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RING_IO_METADATA) --extern anonlib=$(RING_IO_RLIB) -o $@

RING_IO_OBJECTS := \
	$(RING_IO_OBJECT_DIR)/mrt-entry.o \
	$(RING_IO_OBJECT_DIR)/mrt-start.o \
	$(RING_IO_OBJECT_DIR)/mrt-start_info.o \
	$(RING_IO_OBJECT_DIR)/mrt-lifecycle.o \
	$(RING_IO_OBJECT_DIR)/mrt-process.o \
	$(RING_IO_OBJECT_DIR)/mrt-component.o \
	$(RING_IO_OBJECT_DIR)/mrt-entry_rust.o \
	$(RING_IO_ANONLIB) \
	$(RING_IO_OBJECT)

$(RING_IO_SOYO): $(RING_IO_OBJECTS) $(RING_IO_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RING_IO_MANIFEST) -o $@ \
		$(RING_IO_OBJECTS)

$(SOCKET_RING_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(SOCKET_RING_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(SOCKET_RING_OBJECT_DIR)/mrt-%.o: $(REPO_ROOT)/native/mrt/src/%.c $(SOCKET_RING_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(SOCKET_RING_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(SOCKET_RING_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(SOCKET_RING_RLIB): $(ANONLIB_SOURCES) $(SOCKET_RING_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(SOCKET_RING_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(SOCKET_RING_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_SOCKET_RING_METADATA) -o $@

$(SOCKET_RING_ANONLIB): $(ANONLIB_SOURCES) $(SOCKET_RING_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(SOCKET_RING_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(SOCKET_RING_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_SOCKET_RING_METADATA) -o $@

$(SOCKET_RING_OBJECT): $(SOCKET_RING_DIR)/main.rs $(SOCKET_RING_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(SOCKET_RING_OBJECT_DIR)
	$(RUSTC) $(SOCKET_RING_DIR)/main.rs --crate-name soyo_socket_ring \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(SOCKET_RING_METADATA) --extern anonlib=$(SOCKET_RING_RLIB) -o $@

SOCKET_RING_OBJECTS := \
	$(SOCKET_RING_OBJECT_DIR)/mrt-entry.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-start.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-start_info.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-lifecycle.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-process.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-component.o \
	$(SOCKET_RING_OBJECT_DIR)/mrt-entry_rust.o \
	$(SOCKET_RING_ANONLIB) \
	$(SOCKET_RING_OBJECT)

$(SOCKET_RING_SOYO): $(SOCKET_RING_OBJECTS) $(SOCKET_RING_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(SOCKET_RING_MANIFEST) -o $@ \
		$(SOCKET_RING_OBJECTS)

$(DEVICE_RING_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(DEVICE_RING_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(DEVICE_RING_OBJECT_DIR)/mrt-%.o: $(REPO_ROOT)/native/mrt/src/%.c $(DEVICE_RING_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(DEVICE_RING_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(DEVICE_RING_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(DEVICE_RING_RLIB): $(ANONLIB_SOURCES) $(DEVICE_RING_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(DEVICE_RING_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(DEVICE_RING_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_DEVICE_RING_METADATA) -o $@

$(DEVICE_RING_ANONLIB): $(ANONLIB_SOURCES) $(DEVICE_RING_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(DEVICE_RING_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(DEVICE_RING_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=lib --emit=obj \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_DEVICE_RING_METADATA) -o $@

$(DEVICE_RING_OBJECT): $(DEVICE_RING_DIR)/main.rs $(DEVICE_RING_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(DEVICE_RING_OBJECT_DIR)
	$(RUSTC) $(DEVICE_RING_DIR)/main.rs --crate-name soyo_device_ring \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(DEVICE_RING_METADATA) --extern anonlib=$(DEVICE_RING_RLIB) -o $@

DEVICE_RING_OBJECTS := \
	$(DEVICE_RING_OBJECT_DIR)/mrt-entry.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-start.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-start_info.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-lifecycle.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-process.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-component.o \
	$(DEVICE_RING_OBJECT_DIR)/mrt-entry_rust.o \
	$(DEVICE_RING_ANONLIB) \
	$(DEVICE_RING_OBJECT)

$(DEVICE_RING_SOYO): $(DEVICE_RING_OBJECTS) $(DEVICE_RING_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(DEVICE_RING_MANIFEST) -o $@ \
		$(DEVICE_RING_OBJECTS)

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-lifecycle.o: $(REPO_ROOT)/native/mrt/src/lifecycle.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-process.o: $(REPO_ROOT)/native/mrt/src/process.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-entry-c.o: $(REPO_ROOT)/native/mrt/src/entry_c.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(COMPONENT_REPOSITORY_BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-ctype.o: $(REPO_ROOT)/native/ranalib/src/ctype.c $(RANALIB_CTYPE_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-tlsf.o: $(TLSF_SOURCE) $(TLSF_HEADER) $(TLSF_CONFIG) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) \
		-I$(TLSF_DIR)/freestanding -include $(TLSF_CONFIG) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-heap.o: $(REPO_ROOT)/native/ranalib/src/heap.c $(TLSF_HEADER) $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-stdlib.o: $(REPO_ROOT)/native/ranalib/src/stdlib.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(COMPONENT_REPOSITORY_BINDING) $(MRT_HEADER) $(RANALIB_CTYPE_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(COMPONENT_REPOSITORY_OBJECT_DIR)/repository-%.o: $(COMPONENT_REPOSITORY_DIR)/src/%.c $(COMPONENT_REPOSITORY_BINDING) $(COMPONENT_REPOSITORY_DIR)/include/component_repository.h $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(COMPONENT_REPOSITORY_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_INCLUDE_DIR) \
		-I$(COMPONENT_REPOSITORY_DIR)/include $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

COMPONENT_REPOSITORY_OBJECTS := \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-entry.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-start.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-start-info.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-lifecycle.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-process.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/mrt-entry-c.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-ctype.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-errno.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-tlsf.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-heap.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-stdlib.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-string.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/ranalib-stdio.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/repository-path.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/repository-service.o \
	$(COMPONENT_REPOSITORY_OBJECT_DIR)/repository-main.o

$(COMPONENT_REPOSITORY_SOYO): $(COMPONENT_REPOSITORY_OBJECTS) $(COMPONENT_REPOSITORY_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(COMPONENT_REPOSITORY_MANIFEST) -o $@ \
		$(COMPONENT_REPOSITORY_OBJECTS)

$(REPOSITORY_CLIENT_IMAGES_HEADER): $(REPOSITORY_CLIENT_DIR)/images.h.in
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	cp $< $@

$(REPOSITORY_CLIENT_IMAGES_SOURCE): $(COMPONENT_REPOSITORY_SOYO) $(COMPONENT_PLUGIN_SOYO)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	printf '%s\n' '.section .rodata' '.balign 8' \
		'.globl mygo_repository_image_start' 'mygo_repository_image_start:' > $@
	printf '%s\n' '.incbin "$(COMPONENT_REPOSITORY_SOYO)"' \
		'.globl mygo_repository_image_end' 'mygo_repository_image_end:' \
		'.balign 8' '.globl mygo_component_image_start' \
		'mygo_component_image_start:' '.incbin "$(COMPONENT_PLUGIN_SOYO)"' \
		'.globl mygo_component_image_end' 'mygo_component_image_end:' >> $@

$(REPOSITORY_CLIENT_IMAGES_OBJECT): $(REPOSITORY_CLIENT_IMAGES_SOURCE)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(C_BUILD_CONFIG)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-%.o: $(REPO_ROOT)/native/mrt/src/%.c $(REPOSITORY_CLIENT_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(REPOSITORY_CLIENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-%.o: $(REPO_ROOT)/native/ranalib/src/%.c $(REPOSITORY_CLIENT_BINDING) $(C_BUILD_CONFIG)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(REPOSITORY_CLIENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(REPOSITORY_CLIENT_OBJECT_DIR)/repository-path.o: $(COMPONENT_REPOSITORY_DIR)/src/path.c \
		$(COMPONENT_REPOSITORY_DIR)/include/component_repository.h $(C_BUILD_CONFIG)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(COMPONENT_REPOSITORY_DIR)/include \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(REPOSITORY_CLIENT_OBJECT_DIR)/main.o: $(REPOSITORY_CLIENT_DIR)/main.c \
		$(REPOSITORY_CLIENT_BINDING) $(REPOSITORY_CLIENT_IMAGES_HEADER) \
		$(COMPONENT_REPOSITORY_DIR)/include/component_repository.h $(MRT_HEADER) \
		$(RANALIB_STDIO_HEADER) $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(REPOSITORY_CLIENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(REPOSITORY_CLIENT_INCLUDE_DIR) \
		-I$(REPOSITORY_CLIENT_OBJECT_DIR) -I$(COMPONENT_REPOSITORY_DIR)/include \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

REPOSITORY_CLIENT_OBJECTS := \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-entry.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-start.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-start_info.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-lifecycle.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-process.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-component.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/mrt-entry_c.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-ctype.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-errno.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-exit.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-stdlib.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-string.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/ranalib-stdio.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/repository-path.o \
	$(REPOSITORY_CLIENT_OBJECT_DIR)/main.o \
	$(REPOSITORY_CLIENT_IMAGES_OBJECT)

$(REPOSITORY_CLIENT_SOYO): $(REPOSITORY_CLIENT_OBJECTS) $(REPOSITORY_CLIENT_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(REPOSITORY_CLIENT_MANIFEST) -o $@ \
		$(REPOSITORY_CLIENT_OBJECTS)

$(HOST_START_INFO_TEST): $(REPO_ROOT)/native/mrt/tests/start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c $(REPO_ROOT)/native/include/mrt/mrt.h $(BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c -o $@

$(HOST_OPTIONAL_START_INFO_TEST): $(REPO_ROOT)/native/mrt/tests/optional_start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c $(REPO_ROOT)/native/include/mrt/mrt.h \
		$(OPTIONAL_BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(OPTIONAL_INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/optional_start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c -o $@

$(HOST_ABORT_TEST): $(REPO_ROOT)/native/mrt/tests/abort.c \
		$(REPO_ROOT)/native/mrt/src/start.c $(REPO_ROOT)/native/mrt/src/entry_rust.c \
		$(REPO_ROOT)/native/include/mrt/mrt.h $(BINDING)
	mkdir -p $(OBJECT_DIR)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		-Dmain=program_main \
		$(REPO_ROOT)/native/mrt/src/start.c -c -o $(OBJECT_DIR)/mrt-start-host-test.o
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include -Dmain=program_main \
		$(REPO_ROOT)/native/mrt/src/entry_rust.c -c -o $(OBJECT_DIR)/mrt-entry-rust-host-test.o
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/abort.c \
		$(OBJECT_DIR)/mrt-start-host-test.o $(OBJECT_DIR)/mrt-entry-rust-host-test.o -o $@

$(HOST_LIFECYCLE_TEST): $(REPO_ROOT)/native/mrt/tests/lifecycle.c \
		$(REPO_ROOT)/native/mrt/src/lifecycle.c $(REPO_ROOT)/native/include/mrt/mrt.h $(BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/lifecycle.c \
		$(REPO_ROOT)/native/mrt/src/lifecycle.c -o $@

$(HOST_ENTRY_C_TEST): $(REPO_ROOT)/native/mrt/tests/entry_c.c \
		$(REPO_ROOT)/native/mrt/src/entry_c.c $(REPO_ROOT)/native/include/mrt/mrt.h \
		$(BINDING)
	mkdir -p $(OBJECT_DIR)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include -Dmain=program_main \
		$(REPO_ROOT)/native/mrt/src/entry_c.c -c -o $(OBJECT_DIR)/mrt-entry-c-host-test.o
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include $(REPO_ROOT)/native/mrt/tests/entry_c.c \
		$(OBJECT_DIR)/mrt-entry-c-host-test.o -o $@

$(HOST_PROCESS_TEST): $(REPO_ROOT)/native/mrt/tests/process.c \
		$(REPO_ROOT)/native/mrt/src/process.c $(REPO_ROOT)/native/include/mrt/mrt.h \
		$(PROCESS_TEST_C_BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(PROCESS_TEST_INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/process.c \
		$(REPO_ROOT)/native/mrt/src/process.c -o $@

$(HOST_COMPONENT_TEST): $(REPO_ROOT)/native/mrt/tests/component.c \
		$(REPO_ROOT)/native/mrt/src/component.c $(REPO_ROOT)/native/include/mrt/mrt.h \
		$(COMPONENT_TEST_BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(COMPONENT_TEST_INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/component.c \
		$(REPO_ROOT)/native/mrt/src/component.c -o $@

$(HOST_ERRNO_TEST): $(REPO_ROOT)/native/ranalib/tests/errno.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c $(RANALIB_ERRNO_HEADER)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -pthread \
		-I$(REPO_ROOT)/native/include $(REPO_ROOT)/native/ranalib/tests/errno.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_HEAP_TEST): $(REPO_ROOT)/native/ranalib/tests/heap.c \
		$(REPO_ROOT)/native/ranalib/src/heap.c $(REPO_ROOT)/native/ranalib/src/errno.c \
		$(TLSF_SOURCE) $(TLSF_HEADER) $(TLSF_CONFIG) \
		$(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) \
		$(RANALIB_STRING_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror -DRANALIB_HEAP_TEST \
		-I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		-include $(TLSF_CONFIG) \
		$(REPO_ROOT)/native/ranalib/tests/heap.c \
		$(REPO_ROOT)/native/ranalib/src/heap.c \
		$(REPO_ROOT)/native/ranalib/src/string.c \
		$(TLSF_SOURCE) \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_STRING_TEST): $(REPO_ROOT)/native/ranalib/tests/string.c \
		$(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/string.c \
		$(REPO_ROOT)/native/ranalib/src/string.c -o $@

$(HOST_STDIO_TEST): $(REPO_ROOT)/native/ranalib/tests/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/stdio.c $(REPO_ROOT)/native/ranalib/src/errno.c \
		$(REPO_ROOT)/native/ranalib/src/ctype.c $(REPO_ROOT)/native/ranalib/src/stdlib.c \
		$(REPO_ROOT)/native/ranalib/src/string.c $(MRT_HEADER) $(RANALIB_CTYPE_HEADER) \
		$(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(RANALIB_STDLIB_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/ctype.c \
		$(REPO_ROOT)/native/ranalib/src/stdlib.c \
		$(REPO_ROOT)/native/ranalib/src/string.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_STDLIB_TEST): $(REPO_ROOT)/native/ranalib/tests/stdlib.c \
		$(REPO_ROOT)/native/ranalib/src/stdlib.c $(REPO_ROOT)/native/ranalib/src/string.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) \
		$(RANALIB_STRING_HEADER) $(RANALIB_ERRNO_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/stdlib.c \
		$(REPO_ROOT)/native/ranalib/src/stdlib.c \
		$(REPO_ROOT)/native/ranalib/src/string.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_CTYPE_LOCALE_TEST): $(REPO_ROOT)/native/ranalib/tests/ctype_locale.c \
		$(REPO_ROOT)/native/ranalib/src/ctype.c $(REPO_ROOT)/native/ranalib/src/locale.c \
		$(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_CTYPE_HEADER) \
		$(RANALIB_LOCALE_HEADER) $(RANALIB_STRING_HEADER)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/ctype_locale.c \
		$(REPO_ROOT)/native/ranalib/src/ctype.c \
		$(REPO_ROOT)/native/ranalib/src/locale.c \
		$(REPO_ROOT)/native/ranalib/src/string.c -o $@

$(HOST_THREADS_TEST): $(REPO_ROOT)/native/ranalib/tests/threads.c \
		$(REPO_ROOT)/native/ranalib/src/threads.c $(MRT_HEADER) $(RANALIB_THREADS_HEADER) \
		$(RANALIB_STDLIB_HEADER) $(RANALIB_TIME_HEADER) $(PROCESS_TEST_C_BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(PROCESS_TEST_INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/threads.c \
		$(REPO_ROOT)/native/ranalib/src/threads.c -o $@

$(HOST_COMPONENT_REPOSITORY_PATH_TEST): $(COMPONENT_REPOSITORY_DIR)/tests/path.c \
		$(COMPONENT_REPOSITORY_DIR)/src/path.c \
		$(COMPONENT_REPOSITORY_DIR)/include/component_repository.h
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror \
		-I$(COMPONENT_REPOSITORY_DIR)/include \
		$(COMPONENT_REPOSITORY_DIR)/tests/path.c \
		$(COMPONENT_REPOSITORY_DIR)/src/path.c -o $@
