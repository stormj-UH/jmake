
.PHONY: all
all: ; @echo ${.EXTRA_PREREQS}/$@/$</$^/$?/$+/$|/$*/
all: .EXTRA_PREREQS = tick tack
tick tack: ; @echo $@
