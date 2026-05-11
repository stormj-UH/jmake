# Recipe modifier (@, -, +) introduced via variable expansion.
#
# GNU Make strips @, -, + from the EXPANDED recipe text, not just from the
# literal text as written in the Makefile.  When a variable's value begins
# with a modifier character, that modifier must be honoured.
#
# This test covers the Valkey src/Makefile pattern:
#   SERVER_CC = @printf '...' 1>&2; $(CC) ...
#   target:
#       -$(SERVER_CC) ...
# After expansion the recipe line starts with "-@printf ..." and both
# modifiers (- and @) must be stripped before the shell sees the command.

Q = @echo
SILENT_CMD = @echo

.PHONY: all
all: at-only minus-at

.PHONY: at-only
at-only:
	$(Q) hello

# -$(SILENT_CMD) expands to -@echo; both - and @ must be recognised.
# The command is silenced (no echo of the recipe line itself) and
# ignore-error applies.  The word "world" comes from echo's output.
.PHONY: minus-at
minus-at:
	-$(SILENT_CMD) world
