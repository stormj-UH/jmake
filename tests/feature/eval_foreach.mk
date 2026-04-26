# $(eval) within $(foreach)
LIBS := foo bar baz

define make_target
$(1)_LIB := lib$(1).a
endef

$(foreach lib,$(LIBS),$(eval $(call make_target,$(lib))))

all:
	@echo "foo_LIB=$(foo_LIB)"
	@echo "bar_LIB=$(bar_LIB)"
	@echo "baz_LIB=$(baz_LIB)"
