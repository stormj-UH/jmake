# Bare : rules under conditional (the ruby-1.1.5 case)
ENABLE_FEATURE := yes

all: feature-target
	@echo "all done"

ifeq ($(ENABLE_FEATURE),yes)
feature-target:
	@echo "feature enabled"
else
feature-target:
	@echo "feature disabled"
endif
