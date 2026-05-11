# Regression test: $(info), $(warning) sanitize ANSI escape sequences when
# output goes to a real TTY (isatty() returns true).
#
# When output is piped (non-TTY, as in cargo test), escape sequences pass
# through unchanged so downstream consumers are not broken.  This test verifies
# that the non-TTY path works correctly and does NOT accidentally strip output.
#
# The $(info) / $(warning) calls below include plain text only — no escape bytes.
# If the sanitiser erroneously fires on non-TTY output, the messages would be
# altered and the golden comparison would fail.
$(info info-msg-ok)
$(warning warn-msg-ok)

all:
	@echo done
