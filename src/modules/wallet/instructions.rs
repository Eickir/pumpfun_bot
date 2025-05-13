pub struct Buy {
    pub _amount: u64,
    pub _max_sol_cost: u64,
}

impl Buy {
    #[inline(always)]
    pub fn data(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + 8 + 8);
        data.extend_from_slice(&[102, 6, 61, 18, 1, 218, 235, 234]); // discriminator
        data.extend_from_slice(&self._amount.to_le_bytes());
        data.extend_from_slice(&self._max_sol_cost.to_le_bytes());
        data
    }
}

pub struct Sell {
    pub _amount: u64,
    pub _min_sol_output: u64,
}

impl Sell {
    #[inline(always)]
    pub fn data(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + 8 + 8);
        data.extend_from_slice(&[51, 230, 133, 164, 1, 127, 131, 173]); // discriminator
        data.extend_from_slice(&self._amount.to_le_bytes());
        data.extend_from_slice(&self._min_sol_output.to_le_bytes());
        data
    }
}

