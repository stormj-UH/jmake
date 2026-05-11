files1 := $(filter %.o, foo.elc bar.o lose.o)
files2 := $(filter %.o foo.i, foo.i bar.i lose.i foo.elc bar.o lose.o)
all: ; @echo '$(files1) $(files2)'
