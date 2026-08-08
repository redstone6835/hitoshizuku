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
RUST_HELLO_DIR := $(REPO_ROOT)/native/examples/rust-hello
RUST_MANIFEST := $(RUST_HELLO_DIR)/program.json
RUST_INCLUDE_DIR := $(INCLUDE_DIR)/rust-hello
RUST_C_BINDING := $(RUST_INCLUDE_DIR)/mygo_program.h
RUST_BINDING := $(RUST_INCLUDE_DIR)/mygo_program.rs
RUST_OBJECT_DIR := $(OBJECT_DIR)/rust-hello
ANONLIB_SOURCE := $(REPO_ROOT)/native/anonlib/src/lib.rs
ANONLIB_MANIFEST := $(REPO_ROOT)/native/anonlib/Cargo.toml
MRT_HEADER := $(REPO_ROOT)/native/include/mrt/mrt.h
RANALIB_ERRNO_HEADER := $(REPO_ROOT)/native/include/ranalib/errno.h
RANALIB_STDLIB_HEADER := $(REPO_ROOT)/native/include/ranalib/stdlib.h
RANALIB_UNISTD_HEADER := $(REPO_ROOT)/native/include/ranalib/unistd.h

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
RUST_HELLO_METADATA := mygo_native_rust_hello

ifeq ($(ARCH),riscv64)
SOYO_TARGET := riscv64
TARGET_TRIPLE := riscv64-unknown-none-elf
TARGET_FLAGS := -mno-relax -msmall-data-limit=0 -mcmodel=medany
RUST_TARGET_TRIPLE := riscv64imac-unknown-none-elf
ENTRY_SOURCE := $(REPO_ROOT)/native/mrt/arch/riscv64/entry.S
else ifeq ($(ARCH),loongarch64)
SOYO_TARGET := loongarch64
TARGET_TRIPLE := loongarch64-unknown-none
TARGET_FLAGS :=
RUST_TARGET_TRIPLE := loongarch64-unknown-none
ENTRY_SOURCE := $(REPO_ROOT)/native/mrt/arch/loongarch64/entry.S
else
$(error ARCH 必须为 riscv64 或 loongarch64)
endif

MRT_OBJECTS := \
	$(OBJECT_DIR)/mrt-entry.o \
	$(OBJECT_DIR)/mrt-start.o \
	$(OBJECT_DIR)/mrt-start-info.o
RANALIB_OBJECTS := \
	$(OBJECT_DIR)/ranalib-errno.o \
	$(OBJECT_DIR)/ranalib-exit.o \
	$(OBJECT_DIR)/ranalib-write.o
HELLO_OBJECT := $(OBJECT_DIR)/hello.o
HELLO_SOYO := $(OUTPUT)/hello.soyo
RUST_MRT_OBJECTS := \
	$(OBJECT_DIR)/mrt-entry.o \
	$(RUST_OBJECT_DIR)/mrt-start.o \
	$(RUST_OBJECT_DIR)/mrt-start-info.o
ANONLIB_RLIB := $(RUST_OBJECT_DIR)/libanonlib.rlib
ANONLIB_OBJECT := $(RUST_OBJECT_DIR)/anonlib.o
RUST_HELLO_OBJECT := $(RUST_OBJECT_DIR)/rust-hello.o
RUST_HELLO_SOYO := $(OUTPUT)/rust-hello.soyo
C_BUILD_CONFIG := $(OBJECT_DIR)/c-build-config.stamp
RUST_BUILD_CONFIG := $(RUST_OBJECT_DIR)/rust-build-config.stamp
HOST_START_INFO_TEST := $(OUTPUT)/mrt-start-info-test
HOST_WRITE_TEST := $(OUTPUT)/ranalib-write-test

.PHONY: hello rust-hello mrt test FORCE

FORCE:

hello: $(HELLO_SOYO)

rust-hello: $(RUST_HELLO_SOYO)

mrt: $(MRT_OBJECTS)

test: $(HOST_START_INFO_TEST) $(HOST_WRITE_TEST) $(RUST_BINDING)
	$(HOST_START_INFO_TEST)
	$(HOST_WRITE_TEST)
	MYGO_PROGRAM_RS=$(abspath $(RUST_BINDING)) \
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

$(RUST_C_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
		--emit-c-header $@

$(RUST_BINDING): $(SOYO_LD) $(RUST_MANIFEST)
	mkdir -p $(RUST_INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) \
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

$(RUST_OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
		$(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(RUST_OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(RUST_C_BINDING) $(MRT_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(RUST_OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) -I$(RUST_INCLUDE_DIR) \
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

$(OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(BINDING) $(RANALIB_ERRNO_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-write.o: $(REPO_ROOT)/native/ranalib/src/write.c $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_UNISTD_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_OBJECT): $(REPO_ROOT)/native/examples/hello/main.c $(BINDING) $(RANALIB_UNISTD_HEADER) $(C_BUILD_CONFIG)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_SOYO): $(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT) $(MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(MANIFEST) -o $@ \
		$(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT)

$(RUST_HELLO_SOYO): $(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_HELLO_OBJECT) \
		$(RUST_MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(RUST_MANIFEST) -o $@ \
		$(RUST_MRT_OBJECTS) $(ANONLIB_OBJECT) $(RUST_HELLO_OBJECT)

$(HOST_START_INFO_TEST): $(REPO_ROOT)/native/mrt/tests/start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c $(REPO_ROOT)/native/include/mrt/mrt.h $(BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/mrt/tests/start_info.c \
		$(REPO_ROOT)/native/mrt/src/start_info.c -o $@

$(HOST_WRITE_TEST): $(REPO_ROOT)/native/ranalib/tests/write.c \
		$(REPO_ROOT)/native/ranalib/src/write.c $(REPO_ROOT)/native/ranalib/src/errno.c \
		$(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_UNISTD_HEADER) $(BINDING)
	$(HOST_CC) -std=c11 -Wall -Wextra -Werror -I$(INCLUDE_DIR) \
		-I$(REPO_ROOT)/native/include \
		$(REPO_ROOT)/native/ranalib/tests/write.c \
		$(REPO_ROOT)/native/ranalib/src/write.c \
		$(REPO_ROOT)/native/ranalib/src/errno.c -o $@
