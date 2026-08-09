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
RUST_HELLO_DIR := $(REPO_ROOT)/native/examples/rust-hello
RUST_MANIFEST := $(RUST_HELLO_DIR)/program.json
RUST_CHILD_DIR := $(REPO_ROOT)/native/examples/rust-child
RUST_CHILD_MANIFEST := $(RUST_CHILD_DIR)/program.json
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
ANONLIB_SOURCE := $(REPO_ROOT)/native/anonlib/src/lib.rs
ANONLIB_MANIFEST := $(REPO_ROOT)/native/anonlib/Cargo.toml
MRT_HEADER := $(REPO_ROOT)/native/include/mrt/mrt.h
RANALIB_ERRNO_HEADER := $(REPO_ROOT)/native/include/ranalib/errno.h
RANALIB_STDLIB_HEADER := $(REPO_ROOT)/native/include/ranalib/stdlib.h
RANALIB_STRING_HEADER := $(REPO_ROOT)/native/include/ranalib/string.h
RANALIB_STDIO_HEADER := $(REPO_ROOT)/native/include/ranalib/stdio.h

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
RUST_HELLO_METADATA := mygo_native_rust_hello
RUST_PARENT_METADATA := mygo_native_rust_parent

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
	$(OBJECT_DIR)/mrt-entry-c.o
RANALIB_OBJECTS := \
	$(OBJECT_DIR)/ranalib-errno.o \
	$(OBJECT_DIR)/ranalib-exit.o \
	$(OBJECT_DIR)/ranalib-heap.o \
	$(OBJECT_DIR)/ranalib-string.o \
	$(OBJECT_DIR)/ranalib-stdio.o
HELLO_OBJECT := $(OBJECT_DIR)/hello.o
HELLO_SOYO := $(OUTPUT)/hello.soyo
RUST_MRT_OBJECTS := \
	$(OBJECT_DIR)/mrt-entry.o \
	$(RUST_OBJECT_DIR)/mrt-start.o \
	$(RUST_OBJECT_DIR)/mrt-start-info.o \
	$(RUST_OBJECT_DIR)/mrt-lifecycle.o \
	$(RUST_OBJECT_DIR)/mrt-process.o \
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
RUST_CHILD_OBJECT := $(RUST_OBJECT_DIR)/rust-child.o
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
HOST_ERRNO_TEST := $(OUTPUT)/ranalib-errno-test
HOST_HEAP_TEST := $(OUTPUT)/ranalib-heap-test
HOST_STRING_TEST := $(OUTPUT)/ranalib-string-test
HOST_STDIO_TEST := $(OUTPUT)/ranalib-stdio-test

.PHONY: hello rust-hello native-parent native-rust-parent native-parents mrt test FORCE

FORCE:

hello: $(HELLO_SOYO)

rust-hello: $(RUST_HELLO_SOYO)

native-parent: $(PARENT_C_SOYO)

native-rust-parent: $(RUST_PARENT_SOYO)

native-parents: $(CHILD_C_SOYO) $(PARENT_C_SOYO) $(RUST_CHILD_SOYO) $(RUST_PARENT_SOYO)

mrt: $(MRT_OBJECTS)

test: $(HOST_START_INFO_TEST) $(HOST_OPTIONAL_START_INFO_TEST) $(HOST_ABORT_TEST) $(HOST_LIFECYCLE_TEST) $(HOST_ENTRY_C_TEST) $(HOST_PROCESS_TEST) $(HOST_ERRNO_TEST) $(HOST_HEAP_TEST) $(HOST_STRING_TEST) $(HOST_STDIO_TEST) $(PROCESS_TEST_RUST_BINDING)
	$(HOST_START_INFO_TEST)
	$(HOST_OPTIONAL_START_INFO_TEST)
	$(HOST_ABORT_TEST)
	$(HOST_LIFECYCLE_TEST)
	$(HOST_ENTRY_C_TEST)
	$(HOST_PROCESS_TEST)
	$(HOST_ERRNO_TEST)
	$(HOST_HEAP_TEST)
	$(HOST_STRING_TEST)
	$(HOST_STDIO_TEST)
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
				'rust_hello_metadata=$(RUST_HELLO_METADATA)'; \
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

$(RUST_C_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
		--emit-c-header $@

$(RUST_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
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

$(CHILD_C_OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(CHILD_C_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(CHILD_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(CHILD_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(CHILD_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(PARENT_C_BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(PARENT_C_BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(PARENT_C_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(PARENT_C_INCLUDE_DIR) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(PARENT_C_BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
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

$(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o: $(REPO_ROOT)/native/mrt/src/entry_rust.c $(RUST_PARENT_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_PARENT_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(ANONLIB_RLIB): $(ANONLIB_SOURCE) $(RUST_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_METADATA) -o $@

$(ANONLIB_OBJECT): $(ANONLIB_SOURCE) $(RUST_BINDING) $(RUST_BUILD_CONFIG)
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

$(RUST_CHILD_OBJECT): $(RUST_CHILD_DIR)/main.rs $(ANONLIB_RLIB) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(RUSTC) $(RUST_CHILD_DIR)/main.rs --crate-name soyo_rust_child \
		--crate-type=bin --emit=obj --target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(RUST_HELLO_METADATA) --extern anonlib=$(ANONLIB_RLIB) -o $@

$(RUST_PARENT_OBJECT_DIR)/libanonlib.rlib: $(ANONLIB_SOURCE) $(RUST_PARENT_BINDING) $(RUST_BUILD_CONFIG)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	MYGO_PROGRAM_RS=$(abspath $(RUST_PARENT_BINDING)) \
		$(RUSTC) $(ANONLIB_SOURCE) --crate-name anonlib --crate-type=rlib \
		--target=$(RUST_TARGET_TRIPLE) $(COMMON_RUSTFLAGS) \
		-C metadata=$(ANONLIB_PARENT_METADATA) -o $@

$(RUST_PARENT_OBJECT_DIR)/anonlib.o: $(ANONLIB_SOURCE) $(RUST_PARENT_BINDING) $(RUST_BUILD_CONFIG)
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

$(OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-heap.o: $(REPO_ROOT)/native/ranalib/src/heap.c $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-string.o: $(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-stdio.o: $(REPO_ROOT)/native/ranalib/src/stdio.c $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(C_BUILD_CONFIG)
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
		$(CHILD_C_OBJECT_DIR)/ranalib-errno.o $(CHILD_C_OBJECT_DIR)/ranalib-exit.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-stdio.o $(CHILD_C_OBJECT) $(CHILD_C_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(CHILD_C_MANIFEST) -o $@ \
		$(CHILD_C_OBJECT_DIR)/mrt-entry.o $(CHILD_C_OBJECT_DIR)/mrt-start.o \
		$(CHILD_C_OBJECT_DIR)/mrt-start-info.o $(CHILD_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(CHILD_C_OBJECT_DIR)/mrt-process.o $(CHILD_C_OBJECT_DIR)/mrt-entry-c.o \
		$(CHILD_C_OBJECT_DIR)/ranalib-errno.o $(CHILD_C_OBJECT_DIR)/ranalib-exit.o \
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
		$(PARENT_C_OBJECT_DIR)/ranalib-errno.o $(PARENT_C_OBJECT_DIR)/ranalib-exit.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o $(PARENT_C_OBJECT) \
		$(PARENT_C_CHILD_IMAGE_OBJECT) $(PARENT_C_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(PARENT_C_MANIFEST) -o $@ \
		$(PARENT_C_OBJECT_DIR)/mrt-entry.o $(PARENT_C_OBJECT_DIR)/mrt-start.o \
		$(PARENT_C_OBJECT_DIR)/mrt-start-info.o $(PARENT_C_OBJECT_DIR)/mrt-lifecycle.o \
		$(PARENT_C_OBJECT_DIR)/mrt-process.o $(PARENT_C_OBJECT_DIR)/mrt-entry-c.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-errno.o $(PARENT_C_OBJECT_DIR)/ranalib-exit.o \
		$(PARENT_C_OBJECT_DIR)/ranalib-stdio.o $(PARENT_C_OBJECT) \
		$(PARENT_C_CHILD_IMAGE_OBJECT)

$(RUST_CHILD_SOYO): $(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_CHILD_OBJECT) \
		$(RUST_CHILD_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_CHILD_MANIFEST) -o $@ \
		$(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_CHILD_OBJECT)

$(RUST_PARENT_CHILD_IMAGE_SOURCE): $(RUST_CHILD_SOYO)
	mkdir -p $(RUST_PARENT_OBJECT_DIR)
	printf '%s\n' '.section .rodata' '.balign 8' '.globl mygo_child_image_start' 'mygo_child_image_start:' > $@
	printf '%s\n' '.incbin "$(RUST_CHILD_SOYO)"' '.globl mygo_child_image_end' 'mygo_child_image_end:' >> $@

$(RUST_PARENT_CHILD_IMAGE_OBJECT): $(RUST_PARENT_CHILD_IMAGE_SOURCE)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_PARENT_SOYO): \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry.o $(RUST_PARENT_OBJECT_DIR)/mrt-start.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-start-info.o $(RUST_PARENT_OBJECT_DIR)/mrt-lifecycle.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-process.o $(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o \
		$(RUST_PARENT_OBJECT_DIR)/anonlib.o $(RUST_PARENT_OBJECT) \
		$(RUST_PARENT_CHILD_IMAGE_OBJECT) $(RUST_PARENT_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_PARENT_MANIFEST) -o $@ \
		$(RUST_PARENT_OBJECT_DIR)/mrt-entry.o $(RUST_PARENT_OBJECT_DIR)/mrt-start.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-start-info.o $(RUST_PARENT_OBJECT_DIR)/mrt-lifecycle.o \
		$(RUST_PARENT_OBJECT_DIR)/mrt-process.o $(RUST_PARENT_OBJECT_DIR)/mrt-entry-rust.o \
		$(RUST_PARENT_OBJECT_DIR)/anonlib.o $(RUST_PARENT_OBJECT) \
		$(RUST_PARENT_CHILD_IMAGE_OBJECT)

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

$(HOST_ERRNO_TEST): $(REPO_ROOT)/native/ranalib/tests/errno.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c $(RANALIB_ERRNO_HEADER)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -pthread \
		-I$(REPO_ROOT)/native/include $(REPO_ROOT)/native/ranalib/tests/errno.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_HEAP_TEST): $(REPO_ROOT)/native/ranalib/tests/heap.c \
		$(REPO_ROOT)/native/ranalib/src/heap.c $(REPO_ROOT)/native/ranalib/src/errno.c \
		$(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDLIB_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/heap.c \
		$(REPO_ROOT)/native/ranalib/src/heap.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@

$(HOST_STRING_TEST): $(REPO_ROOT)/native/ranalib/tests/string.c \
		$(REPO_ROOT)/native/ranalib/src/string.c $(RANALIB_STRING_HEADER)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/string.c \
		$(REPO_ROOT)/native/ranalib/src/string.c -o $@

$(HOST_STDIO_TEST): $(REPO_ROOT)/native/ranalib/tests/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/stdio.c $(REPO_ROOT)/native/ranalib/src/errno.c \
		$(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_STDIO_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -fno-builtin -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/stdio.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@
