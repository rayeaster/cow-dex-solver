mod paraswap_solver;
pub mod solver_utils;
pub mod zeroex_solver;
use crate::models::batch_auction_model::ExecutedOrderModel;
use crate::models::batch_auction_model::ExecutionPlan;
use crate::models::batch_auction_model::InteractionData;
use crate::models::batch_auction_model::OrderModel;
use crate::models::batch_auction_model::SettledBatchAuctionModel;
use crate::models::batch_auction_model::TokenAmount;
use crate::models::batch_auction_model::{BatchAuctionModel, TokenInfoModel};
use crate::solve::paraswap_solver::ParaswapSolver;
use crate::token_list::get_buffer_tradable_token_list;
use crate::token_list::BufferTradingTokenList;
use crate::token_list::Token;

use self::paraswap_solver::get_sub_trades_from_paraswap_price_response;
use crate::solve::paraswap_solver::api::Root;
use crate::solve::solver_utils::Slippage;
use crate::solve::zeroex_solver::api::SwapQuery;
use crate::solve::zeroex_solver::api::SwapResponse;
use crate::solve::zeroex_solver::ZeroExSolver;
use anyhow::{anyhow, Result};
use ethcontract::batch::CallBatch;
use ethcontract::prelude::*;
use futures::future::join_all;
use primitive_types::{H160, U256};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::time::Duration;

// https://docs.rs/ethcontract/latest/ethcontract/macro.contract.html
ethcontract::contract!("npm:@openzeppelin/contracts@4.2.0/build/contracts/ERC20.json", contract = ERC20);
ethcontract::contract!("https://gist.githubusercontent.com/rayeaster/8969406cd77d1b47b6a22172dccdd331/raw/5fc47d880947ffb690f8abeb9c522546163b4839/testVaultAbi.json", contract = Vault);

lazy_static! {
	
    // NOOB: Better place to define a set of badger vaults addresses?
    pub static ref BADGER_VAULTS: HashSet<H160> = HashSet::from([
          "0xe4cb7cfd027c024aca339026b1e70ff68f82305b".parse().unwrap() // vTERC
    ]);
}

fn is_badger_vault_trade(order: OrderModel) -> bool {
    // For now we are only doing deposits so badger vault tokens are buy_token only
    return BADGER_VAULTS.contains(&order.buy_token);
}

pub async fn solve(
    BatchAuctionModel {
        orders, mut tokens, amms, metadata, ..
    }: BatchAuctionModel,
) -> Result<SettledBatchAuctionModel> {
    //tracing::info!("Before filtering: Solving instance with the orders {:?} and the tokens: {:?} and amms: {:?} and metadata: {:?}", orders, tokens, amms, metadata);

    // Filter badger vault tokens orders
    let badger_vault_orders: BTreeMap<usize, OrderModel> = orders.into_iter().filter(|(_, order)| is_badger_vault_trade(order.clone())).collect();
  
    // If there aren't any badger vault trades, return empty.
    if badger_vault_orders.is_empty() {
       tracing::info!("No badger vault order to solve");
       return Ok(SettledBatchAuctionModel::default());
    } else{
       tracing::info!("Badger vault orders: {:?}", badger_vault_orders);
       //return Ok(SettledBatchAuctionModel::default());
    } 
  
    let http = Http::new("https://rpc.gnosischain.com").unwrap();
    let web3 = Web3::new(http);
    let mut solution = SettledBatchAuctionModel::default();   
  
    let mut order_id: usize = 0;
	for (_, order) in &badger_vault_orders {
          
        let token: ERC20 = ERC20::at(&web3, order.sell_token);
        let vault: Vault = Vault::at(&web3, order.buy_token);

        let token_name: String = token.name().call().await.unwrap();
        let vault_name: String = vault.name().call().await.unwrap();
        tracing::info!("Processing trade from: {:?} to {:?}", token_name, vault_name);

        let token_decimals = token.decimals().call().await.unwrap();
        let vault_decimals = vault.decimals().call().await.unwrap();
        let vault_pps = vault.get_price_per_full_share().call().await.unwrap();//e18

        // NOOB: direct calculation by share ratio
        let can_buy_amount = (order.sell_amount / U256::exp10(token_decimals as usize)) * U256::exp10(vault_decimals as usize) * (U256::exp10(18) / vault_pps);

        // User might be asking for more than what they can afford
        if order.buy_amount > can_buy_amount {
            tracing::info!("Couldn't do trade. Trader wants: {:?} and we can convert to {:?}", order.buy_amount, can_buy_amount);
            continue;
        }

        //let settlement_contract_address: H160 = "0x9008d19f58aabd9ed0d60971565aa8510560ab41".parse().unwrap();        

        let approve_method = token.approve(order.buy_token, order.sell_amount);
        let approve_calldata = approve_method.tx.data.expect("no calldata").0;
        let approve_interaction_item = InteractionData {
            target: order.sell_token,
            value: 0.into(),
            call_data: approve_calldata, 
            exec_plan: None,
            inputs: vec![],
            outputs: vec![],
        };
        solution.interaction_data.push(approve_interaction_item);

        let deposit_method = vault.deposit(order.sell_amount);
        let deposit_calldata = deposit_method.tx.data.expect("no calldata").0;
        let deposit_interaction_item = InteractionData {
            target: order.buy_token,
            value: 0.into(),
            call_data: deposit_calldata,
            exec_plan: None,
            inputs: vec![],
            outputs: vec![],
        };

        solution.interaction_data.push(deposit_interaction_item);
          
        // HACK
        solution.prices = HashMap::new();
        solution.prices.insert(order.sell_token, U256::one());
        solution.prices.insert(order.buy_token, U256::one());
          
        solution.orders.insert(
            order_id, // NOOB: order_id++ doesn't work in rust?
            ExecutedOrderModel {
                exec_sell_amount: order.sell_amount,
                exec_buy_amount: can_buy_amount,
            },
        );

        order_id = order_id + 1;

    }
  
    tracing::info!("Found solution: {:?}", solution);
    return Ok(solution);
}

#[derive(Clone, Debug)]
pub struct SubTrade {
    pub src_token: H160,
    pub dest_token: H160,
    pub src_amount: U256,
    pub dest_amount: U256,
}
fn overwrite_eth_with_weth_token(token: H160) -> H160 {
    if token.eq(&"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".parse().unwrap()) {
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".parse().unwrap()
    } else {
        token
    }
}
