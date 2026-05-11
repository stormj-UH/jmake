f = echo $(1)
t:; @$(call f,"a \
            b"); \
        $(call f,"a \
            b")
