interface PumpFunSwapEvent {
  type: string; 
  mint: string; 
  signer: string; 
  bondingCurve: string; 
  solPoolAmount: number;
  tokenPoolAmount: number; 
  bondingCurveProgress: string; 
}

export function parsedPumpfunTransaction(parsedTxn) : PumpFunSwapEvent {
    const innerInstruction = parsedTxn.instructions.pumpFunIxs;
    const innerEvent = parsedTxn.instructions.events[0].data;
    const swapInstruction = innerInstruction.find((x)=> x.name == "buy" || x.name == "sell");
    const instructionName = swapInstruction?.name;
    const bondingCurve = swapInstruction?.accounts.find((x)=> x.name =='bonding_curve').pubkey;
    const mint = swapInstruction?.accounts.find((x)=> x.name == 'mint').pubkey;
    const user = swapInstruction?.accounts.find((x)=> x.name == 'user').pubkey;
    const solReserves = innerEvent.real_sol_reserves/1_000_000_000; 
    const tokenReserves = innerEvent.real_token_reserves;
    const curvePercentageCalculation = calculate_curve_percentage(solReserves, 84);
    const output : PumpFunSwapEvent = {
        type : instructionName,
        mint : mint,
        signer : user,
        bondingCurve : bondingCurve,
        solPoolAmount : solReserves,
        tokenPoolAmount : tokenReserves,
        bondingCurveProgress : curvePercentageCalculation + "%"
    }
   return output;
 }

function calculate_curve_percentage(currentValue : number, maxValue : number) : number {
   let percentage = 100;
   return parseFloat(((Number(currentValue) / Number(maxValue)) * percentage).toFixed(1));
}