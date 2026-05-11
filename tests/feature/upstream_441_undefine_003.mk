define undef
$(eval undefine $$1)
endef

a := a
$(call undef,a)
$(info $(flavor a))


all: ;@:
