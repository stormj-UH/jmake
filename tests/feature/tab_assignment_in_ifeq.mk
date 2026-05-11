# Tab-indented variable assignments inside ifeq/else blocks that appear after
# a rule definition must be treated as assignments, NOT recipe lines.
#
# This is the Valkey src/Makefile pattern (lines 145 and 166):
#
#   FINAL_LIBS=-lm
#   ifeq (clang,$(CLANG))
#           FINAL_LIBS+=-latomic       # 8 spaces (works, not tab-indented)
#   else
#   ifeq ($(uname_S),Linux)
#   	FINAL_LIBS+= -ldl ...           # TAB — was wrongly treated as recipe
#   endif
#   endif
#
# When a rule is followed by an ifeq block, GNU Make 4.x recognises tab-indented
# assignment lines inside the ifeq/else/endif as variable assignments.

VAR :=

.PHONY: all
all:
	@echo "VAR=$(VAR)"

# A preceding rule — this leaves in_recipe=true in the parser state.
# The ifeq block that follows must still treat tab-indented assignments
# as assignments, not as recipe lines of the 'something' rule.
.PHONY: something
something:
	@echo something

# ifeq block following the rule: the tab-indented VAR += must be an assignment.
ifeq (yes,yes)
	VAR += foo
endif
