tst = $(eval export X123)
$(call tst)
all: ; @echo "$${X123-not set}"
