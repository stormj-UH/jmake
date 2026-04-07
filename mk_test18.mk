.SECONDEXPANSION:
dep:=hello.z
all: hello.tsk
%.tsk: $$(dep) ; @echo $@
%.z : %.x ; @echo $@
