# patsubst chains (AOBJS pattern), srcdir variants
srcdir := .
SRCS := foo.c bar.c baz.c
OBJS := $(patsubst $(srcdir)/%.c,$(srcdir)/%.o,$(SRCS))
OBJS2 := $(patsubst %.c,%.o,$(SRCS))

srcdir2 := foo
SRCS2 := foo/a.c foo/b.c
OBJS3 := $(patsubst $(srcdir2)/%.c,$(srcdir2)/%.o,$(SRCS2))

all:
	@echo "OBJS=$(OBJS)"
	@echo "OBJS2=$(OBJS2)"
	@echo "OBJS3=$(OBJS3)"
