files1 := $(filter-out %.o, foo.elc bar.o lose.o)
files2 := $(filter-out foo.i bar.i lose.i %.o, foo.i bar.i lose.i foo.elc bar.o lose.o)
all: ; @echo '$(files1) $(files2)'
