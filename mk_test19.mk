.SECONDEXPANSION:
dep:=.z
all: hello.tsk
%.tsk: %$$(dep) ; @echo $@
%.z : %.x ; @echo $@
