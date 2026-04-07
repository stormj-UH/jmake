.SECONDEXPANSION:

.PHONY: foo bar
foo: bar
foo: $$(info $$<)
%oo: ;
