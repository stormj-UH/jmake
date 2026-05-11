words := foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10 foo.1 foo.2 foo.3 foo.4 foo.5 foo.6 foo.7 foo.8 foo.9 foo.10
files1 := $(filter %.3, $(words))
files2 := $(filter %.4 foo.5 foo.6, $(words))
all: ; @echo '$(files1) $(files2)'
