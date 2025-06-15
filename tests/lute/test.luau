print("vm.spawn called")
return {
    l = function()
        --error("This is an error message")
        local table = {}
        for i = 1, 1000 do
            print(i)
            table[i] = i * 2
        end

        return 13 * 6
    end,
}