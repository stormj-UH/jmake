# Tab-indented := (Simple/immediate) assignments inside ifeq blocks must expand
# their RHS immediately at assignment time, not store a literal $(VAR) reference.
#
# This is the Valkey src/Makefile pattern (line 295):
#
#   FINAL_LIBS := -lm
#   ifeq ($(MALLOC),jemalloc)
#   	FINAL_LIBS := ../deps/jemalloc/lib/libjemalloc.a $(FINAL_LIBS)
#   endif
#
# Without the fix, jmake stored FINAL_LIBS verbatim as:
#   "../deps/jemalloc/lib/libjemalloc.a $(FINAL_LIBS)"
# creating a self-referential recursive variable. When the LINK recipe expanded
# $(FINAL_LIBS), the shell received the literal subshell $(FINAL_LIBS) and tried
# to run `FINAL_LIBS` as a command, emitting:
#   /bin/sh: FINAL_LIBS: inaccessible or not found

MALLOC := jemalloc
FINAL_LIBS := -lm

ifeq ($(MALLOC),jemalloc)
	FINAL_LIBS := prepend $(FINAL_LIBS)
endif

all:
	@echo "FINAL_LIBS=$(FINAL_LIBS)"
