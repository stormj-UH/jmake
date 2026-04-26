# wildcard variants
srcdir := /tmp/wc-test/src

SRCS_DOTSLASH := $(wildcard ./*.mk)
SRCS_ABS := $(wildcard $(srcdir)/*.c)
SRCS_NOPREFIX := $(notdir $(wildcard $(srcdir)/*.c))

all:
	@echo "dotslash count=$(words $(SRCS_DOTSLASH))"
	@echo "abs=$(SRCS_ABS)"
	@echo "noprefix=$(SRCS_NOPREFIX)"
