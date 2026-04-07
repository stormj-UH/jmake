
.EXTRA_PREREQS = tick tack
.PHONY: all
all: ; @echo ${.EXTRA_PREREQS}/$@/$</$^/$?/$+/$|/$*/
tick tack: ; @echo $@
