define FOO
$(foreach
  a 	
 , b	
 c  ,$(info
  $a    )
    )
endef
$(FOO)
all:;@:
