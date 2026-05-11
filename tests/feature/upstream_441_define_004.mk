override BOGUS=1

FOO = foo
NAME = def
def =
ifdef BOGUS
 define  $(subst e,e,$(NAME))     =
  ifeq (1,1)
   FOO = bar
  endif
 endef
endif

$(eval $(def))
all: ; @echo $(FOO)
