# include / -include / sinclude with various forms
EXTRA := extra.mk
-include $(EXTRA)
sinclude nonexistent_also.mk

MSG ?= not-included

all:
	@echo "MSG=$(MSG)"
