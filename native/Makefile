SHELL := /bin/sh

ARCH ?= riscv64
HOST_CC ?= cc
CLANG ?= clang

REPO_ROOT := $(abspath ..)
OUTPUT ?= $(REPO_ROOT)/build/$(ARCH)/native
OBJECT_DIR := $(OUTPUT)/obj
INCLUDE_DIR := $(OUTPUT)/include
BINDING := $(INCLUDE_DIR)/mygo_program.h
MANIFEST := $(REPO_ROOT)/native/examples/hello/program.json
MRT_HEADER := $(REPO_ROOT)/native/include/mrt/mrt.h
RANALIB_ERRNO_HEADER := $(REPO_ROOT)/native/include/ranalib/errno.h
RANALIB_STDLIB_HEADER := $(REPO_ROOT)/native/include/ranalib/stdlib.h
RANALIB_UNISTD_HEADER := $(REPO_ROOT)/native/include/ranalib/unistd.h

HOST_TARGET := x86_64-unknown-linux-gnu
SOYO_LINKER_TARGET := $(REPO_ROOT)/tools/soyo-linker/target
SOYO_LD ?= $(SOYO_LINKER_TARGET)/$(HOST_TARGET)/release/soyo-ld
SOYO_LINKER_INPUTS := $(shell find \
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

ifeq ($(ARCH),riscv64)
SOYO_TARGET := riscv64
TARGET_TRIPLE := riscv64-unknown-none-elf
TARGET_FLAGS := -mno-relax -msmall-data-limit=0 -mcmodel=medany
ENTRY_SOURCE := $(REPO_ROOT)/native/mrt/arch/riscv64/entry.S
else ifeq ($(ARCH),loongarch64)
SOYO_TARGET := loongarch64
TARGET_TRIPLE := loongarch64-unknown-none
TARGET_FLAGS :=
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
HOST_START_INFO_TEST := $(OUTPUT)/mrt-start-info-test
HOST_WRITE_TEST := $(OUTPUT)/ranalib-write-test

.PHONY: hello mrt test

hello: $(HELLO_SOYO)

mrt: $(MRT_OBJECTS)

test: $(HOST_START_INFO_TEST) $(HOST_WRITE_TEST)
	$(HOST_START_INFO_TEST)
	$(HOST_WRITE_TEST)

$(SOYO_LD): $(SOYO_LINKER_INPUTS)
	CARGO_TARGET_DIR=$(SOYO_LINKER_TARGET) \
		cargo build --manifest-path $(REPO_ROOT)/tools/soyo-linker/Cargo.toml \
		--target $(HOST_TARGET) --release

$(BINDING): $(SOYO_LD) $(MANIFEST)
	mkdir -p $(INCLUDE_DIR)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(MANIFEST) \
		--emit-c-header $@

$(OBJECT_DIR)/mrt-entry.o: $(ENTRY_SOURCE) $(BINDING)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_ASFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-start.o: $(REPO_ROOT)/native/mrt/src/start.c $(BINDING) $(MRT_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/mrt-start-info.o: $(REPO_ROOT)/native/mrt/src/start_info.c $(BINDING) $(MRT_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-errno.o: $(REPO_ROOT)/native/ranalib/src/errno.c $(BINDING) $(RANALIB_ERRNO_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-exit.o: $(REPO_ROOT)/native/ranalib/src/exit.c $(BINDING) $(MRT_HEADER) $(RANALIB_STDLIB_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(OBJECT_DIR)/ranalib-write.o: $(REPO_ROOT)/native/ranalib/src/write.c $(BINDING) $(MRT_HEADER) $(RANALIB_ERRNO_HEADER) $(RANALIB_UNISTD_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_OBJECT): $(REPO_ROOT)/native/examples/hello/main.c $(BINDING) $(RANALIB_UNISTD_HEADER)
	mkdir -p $(OBJECT_DIR)
	$(CLANG) --target=$(TARGET_TRIPLE) $(COMMON_CFLAGS) $(TARGET_FLAGS) -c $< -o $@

$(HELLO_SOYO): $(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT) $(MANIFEST) $(SOYO_LD)
	$(SOYO_LD) --target $(SOYO_TARGET) --manifest $(MANIFEST) -o $@ \
		$(MRT_OBJECTS) $(RANALIB_OBJECTS) $(HELLO_OBJECT)

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
