# Static pattern rule prerequisite merge test.
#
# GNU Make supports declaring a static pattern rule in multiple places and
# merges the prerequisites across declarations.  This test covers the case
# where the target-pattern in Declaration #2 does NOT match any of the OBJS
# targets: GNU Make warns "target 'X' doesn't match the target pattern" and
# still associates the recipe with each target so that $< (filled by Decl #1)
# is non-empty.
#
# This is the jemalloc/Valkey pattern:
#   Decl #1 — provides prereq pattern, no recipe
#   Decl #2 — provides recipe, empty prereq (pattern doesn't match targets)

OBJS = src/a.o src/b.o

all: $(OBJS)
	@echo done

# Declaration #1: prereqs only, pattern matches all OBJS.
$(OBJS): src/%.o: src/%.c

# Declaration #2: recipe only; target pattern "out/%.o" does NOT match
# "src/a.o" / "src/b.o".  GNU Make warns and still binds the recipe.
$(OBJS): out/%.o:
	@echo "CC $@ $<"
