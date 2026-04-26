# TAB-prefixed VAR= inside ifndef/ifeq (the dropbear-1.1.3 case)
FOO :=

ifndef FOO
	BAR := set-in-ifndef
else
	BAR := set-in-else
endif

ifndef UNDEFINED_VAR
	BAZ := set-in-baz-ifndef
endif

all:
	@echo "BAR=$(BAR)"
	@echo "BAZ=$(BAZ)"
