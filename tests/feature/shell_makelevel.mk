# $(shell) inheriting MAKELEVEL
LEVEL := $(shell echo $$MAKELEVEL)
all:
	@echo "MAKELEVEL from shell=$(LEVEL)"
	@echo "MAKELEVEL direct=$(MAKELEVEL)"
