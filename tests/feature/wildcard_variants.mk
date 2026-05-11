# wildcard variants
# Exercises $(wildcard) with absolute paths and $(notdir) post-processing.
# The "dotslash" leg previously used $(wildcard ./*.mk) — relying on the
# count of .mk files in tests/feature/ — which became unstable as we
# adopted upstream tests.  Now all three legs read from the controlled
# fixture under /tmp/wc-test so the count is deterministic.
srcdir := /tmp/wc-test/src

SRCS_DOTSLASH := $(wildcard $(srcdir)/*.c)
SRCS_ABS := $(wildcard $(srcdir)/*.c)
SRCS_NOPREFIX := $(notdir $(wildcard $(srcdir)/*.c))

all:
	@echo "dotslash count=$(words $(SRCS_DOTSLASH))"
	@echo "abs=$(SRCS_ABS)"
	@echo "noprefix=$(SRCS_NOPREFIX)"
