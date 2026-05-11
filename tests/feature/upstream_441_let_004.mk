reverse = $(let first rest,$1,$(if $(rest),$(call reverse,$(rest)) )$(first))

all: ; @echo $(call reverse, \
                 moe   miny  meeny eeny \
              )