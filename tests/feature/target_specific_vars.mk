# target-specific vars: literal targets
all: foo bar

foo: CFLAGS := -O2
foo:
	@echo "foo CFLAGS=$(CFLAGS)"

bar: CFLAGS := -g
bar:
	@echo "bar CFLAGS=$(CFLAGS)"

CFLAGS := -Wall
