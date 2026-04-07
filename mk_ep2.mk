
.EXTRA_PREREQS = tick tack
a%: ; @echo ${.EXTRA_PREREQS}/$@/$</$^/$?/$+/$|/$*/
tick tack: ; @echo $@
